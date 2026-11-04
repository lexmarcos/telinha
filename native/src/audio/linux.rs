//! Computer audio on Linux through PipeWire, without Discord.
//!
//! Instead of recording the whole sound output (which includes the voices of
//! people in the call, so everyone hears themselves back with delay), Telinha
//! creates its own input and links to it the output of every app that plays
//! audio, except Discord and its clients (Vesktop and friends). Apps that start
//! or stop audio later join and leave on their own: the PipeWire registry tells
//! us. This is what Windows does with "process loopback" and what Vesktop does
//! in its own screen share.
//!
//! While streaming a single window only the app the person chose is linked. The
//! choice is by the app's identity (see `plan::identity`), so its streams can end
//! and come back, from any of its processes, and be picked up again on their own.
//!
//! TELINHA_SOM_TUDO=1 goes back to the old way: the whole default output. Only
//! for a whole monitor: a window's stream never records the whole output.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::time::Duration;

use pipewire as pw;
use pw::spa;
use pw::types::ObjectType;

use super::plan::{self, AudioApp, Plan, Selection};
use super::{CHANNELS, RATE};

/// Name of our capture node, with the PID so it is not confused with another open Telinha.
fn node_name() -> String {
    format!("telinha-som-{}", std::process::id())
}

/// Name of the node that plays our chimes (left out of the capture).
fn chime_node_name() -> String {
    format!("telinha-aviso-{}", std::process::id())
}

/// Name fragments that identify Discord and its alternative clients.
const DISCORD: [&str; 7] = ["discord", "vesktop", "vencord", "webcord", "armcord", "legcord", "equibop"];

pub fn start(tx: SyncSender<Vec<f32>>, stop: Arc<AtomicBool>, plan: Plan) -> Result<String, String> {
    let everything = plan::whole_output(std::env::var_os("TELINHA_SOM_TUDO").is_some(), &plan);
    let source = match &plan {
        _ if everything => "saída de som padrão (PipeWire)",
        Plan::App { .. } => "só o app escolhido (PipeWire)",
        _ => "todo o som do computador menos o Discord (PipeWire)",
    };
    std::thread::Builder::new()
        .name("telinha-pw-som".into())
        .spawn(move || {
            if let Err(e) = run(tx, stop, everything, plan) {
                tracing::error!("PipeWire audio: {e}");
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(source.into())
}

fn run(tx: SyncSender<Vec<f32>>, stop: Arc<AtomicBool>, everything: bool, plan: Plan) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
    let core = context.connect_rc(None).map_err(|e| e.to_string())?;
    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::NODE_NAME => node_name(),
        *pw::keys::NODE_DESCRIPTION => "Telinha (som da transmissão)",
        // 10 ms chunks, the same as an Opus frame, instead of PipeWire's
        // default 1024 samples (21 ms). While streaming, the graph we are
        // linked to then runs at that quantum unless something asks for
        // less (games and browsers often do): more wake-ups for the apps
        // playing, a common setting that PipeWire handles fine.
        *pw::keys::NODE_LATENCY => "480/48000",
    };
    if everything {
        // Captures what plays on the default output (its monitor).
        props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    } else {
        // Nobody links us anywhere: we make the links ourselves.
        props.insert("node.autoconnect", "false");
    }
    let stream = pw::stream::StreamBox::new(&core, "telinha-som", props).map_err(|e| e.to_string())?;

    let _listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Some(d) = buffer.datas_mut().first_mut() else { return };
            let n = d.chunk().size() as usize / 4;
            let Some(bytes) = d.data() else { return };
            let mut samples: Vec<f32> = bytes[..n * 4].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
            // Recording the whole output also records our chime: silence it.
            if everything && super::chime::muting() {
                samples.fill(0.0);
            }
            let _ = tx.try_send(samples);
        })
        .register()
        .map_err(|e| e.to_string())?;

    // F32 stereo at 48 kHz: PipeWire converts from whatever each app uses.
    let values = format_pod()?;
    let mut params = [spa::pod::Pod::from_bytes(&values).ok_or("formato de som inválido")?];
    let mut flags = pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS;
    if everything {
        flags |= pw::stream::StreamFlags::AUTOCONNECT;
    }
    stream.connect(spa::utils::Direction::Input, None, flags, &mut params).map_err(|e| e.to_string())?;

    // Tracks the apps that play audio and links each one to our input.
    let registry = core.get_registry_rc().map_err(|e| e.to_string())?;
    let (selection, list) = match plan {
        Plan::App { selection, list } => (Some(selection), list),
        _ => (None, None),
    };
    let graph = Rc::new(RefCell::new(Graph { filter: Filter::from_selection(selection.as_ref()), ..Default::default() }));
    let report = move |g: &mut Graph| {
        if let (Some(list), Some(now)) = (&list, g.list_if_changed()) {
            list(now);
        }
    };
    let report_rm = report.clone();
    let _registry_listener = (!everything).then(|| {
        let (g_add, g_rm, core) = (graph.clone(), graph.clone(), core.clone());
        let core_rm = core.clone();
        registry
            .add_listener_local()
            .global(move |obj| {
                let mut g = g_add.borrow_mut();
                if g.learn(obj) {
                    g.reconcile(&core);
                }
                report(&mut g);
            })
            .global_remove(move |id| {
                let mut g = g_rm.borrow_mut();
                g.forget(id);
                g.reconcile(&core_rm);
                report_rm(&mut g);
            })
            .register()
    });

    let ml = mainloop.clone();
    let (g_timer, core_timer) = (graph.clone(), core.clone());
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop.load(Ordering::Relaxed) {
            ml.quit();
        }
        // The person changed which app to hear.
        let wanted = Filter::from_selection(selection.as_ref());
        let mut g = g_timer.borrow_mut();
        if wanted != g.filter {
            tracing::info!(?wanted, "audio: following the app the person chose");
            g.filter = wanted;
            g.reconcile(&core_timer);
        }
    });
    timer.update_timer(Some(Duration::from_millis(100)), Some(Duration::from_millis(100))).into_result().map_err(|e| e.to_string())?;
    mainloop.run();
    Ok(())
}

/// Our audio format (F32 stereo at `RATE`) as a PipeWire parameter.
fn format_pod() -> Result<Vec<u8>, String> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(RATE);
    info.set_channels(CHANNELS as u32);
    let mut pos = [0u32; spa::param::audio::MAX_CHANNELS];
    pos[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    pos[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(pos);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    Ok(spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(obj))
        .map_err(|e| format!("{e:?}"))?
        .0
        .into_inner())
}

/// Plays a short sound (interleaved stereo at `RATE`) on the default output, in the background.
pub fn play(samples: Vec<f32>) {
    let spawned = std::thread::Builder::new().name("telinha-aviso".into()).spawn(move || {
        if let Err(e) = play_now(samples) {
            tracing::warn!("chime: {e}");
        }
    });
    if let Err(e) = spawned {
        tracing::warn!("chime: {e}");
    }
}

fn play_now(samples: Vec<f32>) -> Result<(), String> {
    pw::init();
    let len = Duration::from_millis((samples.len() / CHANNELS) as u64 * 1000 / RATE as u64);
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
    let core = context.connect_rc(None).map_err(|e| e.to_string())?;
    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Notification",
        *pw::keys::NODE_NAME => chime_node_name(),
        *pw::keys::NODE_DESCRIPTION => "Telinha (avisos)",
    };
    let stream = pw::stream::StreamBox::new(&core, "telinha-aviso", props).map_err(|e| e.to_string())?;
    let _listener = stream
        .add_local_listener_with_user_data(0usize)
        .process(move |stream, pos| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Some(d) = buffer.datas_mut().first_mut() else { return };
            let Some(bytes) = d.data() else { return };
            // After the sound, silence until the loop ends.
            let n = bytes.len() / 4 / CHANNELS * CHANNELS;
            for (i, out) in bytes[..n * 4].chunks_exact_mut(4).enumerate() {
                out.copy_from_slice(&samples.get(*pos + i).copied().unwrap_or(0.0).to_le_bytes());
            }
            *pos += n;
            let chunk = d.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = (4 * CHANNELS) as _;
            *chunk.size_mut() = (n * 4) as _;
        })
        .register()
        .map_err(|e| e.to_string())?;
    let values = format_pod()?;
    let mut params = [spa::pod::Pod::from_bytes(&values).ok_or("formato de som inválido")?];
    let flags = pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS;
    stream.connect(spa::utils::Direction::Output, None, flags, &mut params).map_err(|e| e.to_string())?;

    // Room for the output buffer to drain before closing.
    let ml = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| ml.quit());
    timer.update_timer(Some(len + Duration::from_millis(300)), None).into_result().map_err(|e| e.to_string())?;
    mainloop.run();
    Ok(())
}

struct Port {
    node: u32,
    output: bool,
    channel: String,
}

/// Which app outputs go into the stream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum Filter {
    /// Everything but Discord (a whole monitor).
    #[default]
    AllButDiscord,
    /// Only this app. `None`: nothing was chosen yet, or "Sem som".
    Only(Option<String>),
}

impl Filter {
    fn from_selection(selection: Option<&Selection>) -> Self {
        selection.map_or(Self::AllButDiscord, |s| Self::Only(s.get()))
    }

    fn includes(&self, app: &Stream) -> bool {
        if app.own {
            return false;
        }
        match self {
            Self::AllButDiscord => !app.discord,
            Self::Only(chosen) => !app.discord && chosen.as_deref() == Some(app.app.id.as_str()),
        }
    }
}

/// An app's audio output.
struct Stream {
    app: AudioApp,
    discord: bool,
    /// Our own chimes (viewers already play theirs).
    own: bool,
}

#[derive(Default)]
struct Graph {
    /// Our capture node.
    me: Option<u32>,
    filter: Filter,
    /// App audio outputs.
    apps: HashMap<u32, Stream>,
    ports: HashMap<u32, Port>,
    /// Links we created (app output, our input). They live as long as the proxy lives.
    links: HashMap<(u32, u32), pw::link::Link>,
    /// The list last reported.
    listed: Option<Vec<AudioApp>>,
}

impl Graph {
    /// Records a new PipeWire object. `true` if there may be a new link to make.
    fn learn(&mut self, obj: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>) -> bool {
        let Some(props) = obj.props else { return false };
        match obj.type_ {
            ObjectType::Node => {
                let class = props.get("media.class").unwrap_or("");
                if props.get("node.name") == Some(node_name().as_str()) {
                    self.me = Some(obj.id);
                    return true;
                }
                if class == "Stream/Output/Audio" {
                    return self.add_stream(obj.id, |k| props.get(k).map(str::to_owned));
                }
                false
            }
            ObjectType::Port => {
                if props.get("port.monitor") == Some("true") {
                    return false;
                }
                let (Some(node), Some(dir)) = (props.get("node.id").and_then(|n| n.parse().ok()), props.get("port.direction")) else { return false };
                let channel = props.get("audio.channel").unwrap_or("MONO").to_owned();
                self.ports.insert(obj.id, Port { node, output: dir == "out", channel });
                true
            }
            _ => false,
        }
    }

    /// An app started playing. `true` if it goes into the stream.
    fn add_stream(&mut self, id: u32, get: impl Fn(&str) -> Option<String>) -> bool {
        let own = get("node.name").as_deref() == Some(chime_node_name().as_str());
        let names = ["application.process.binary", "application.name", "application.id", "node.name"].map(|k| get(k).unwrap_or_default().to_lowercase());
        let discord = names.iter().any(|n| DISCORD.iter().any(|d| n.contains(d)));
        let stream = Stream { app: plan::identity(&get), discord, own };
        let included = self.filter.includes(&stream);
        let why = if own { "Telinha's own chimes" } else if discord { "Discord" } else { "not the chosen app" };
        if included {
            tracing::info!(app = stream.app.label, "audio included in the stream");
        } else {
            tracing::info!(app = stream.app.label, why, "audio left out");
        }
        self.apps.insert(id, stream);
        included
    }

    /// The apps the person can choose from, if that changed since the last call.
    /// Not Discord (the people in the call would hear themselves) nor Telinha.
    fn list_if_changed(&mut self) -> Option<Vec<AudioApp>> {
        let now = plan::app_list(self.apps.values().filter(|s| !s.own && !s.discord).map(|s| s.app.clone()));
        if self.listed.as_ref() == Some(&now) {
            return None;
        }
        self.listed = Some(now.clone());
        Some(now)
    }

    fn forget(&mut self, id: u32) {
        if self.me == Some(id) {
            self.me = None;
        }
        self.apps.remove(&id);
        self.ports.remove(&id);
        self.links.retain(|(out, inp), _| *out != id && *inp != id);
    }

    /// The links there should be right now: (app node, its output port, our
    /// input port). Every allowed app output to our input, channel by channel
    /// (mono and center go to both sides).
    fn wanted(&self) -> Vec<(u32, u32, u32)> {
        let Some(me) = self.me else { return Vec::new() };
        let ours: HashMap<&str, u32> = self.ports.iter().filter(|(_, p)| p.node == me && !p.output).map(|(id, p)| (p.channel.as_str(), *id)).collect();
        if ours.is_empty() {
            return Vec::new();
        }
        let mut wanted = Vec::new();
        for (id, p) in &self.ports {
            if !p.output || !self.apps.get(&p.node).is_some_and(|s| self.filter.includes(s)) {
                continue;
            }
            let sides: &[&str] = match p.channel.as_str() {
                "FL" | "RL" | "SL" | "FLC" => &["FL"],
                "FR" | "RR" | "SR" | "FRC" => &["FR"],
                "MONO" | "FC" => &["FL", "FR"],
                _ => &[],
            };
            for side in sides {
                if let Some(input) = ours.get(side) {
                    wanted.push((p.node, *id, *input));
                }
            }
        }
        wanted
    }

    /// Makes the links match `wanted`: an app no longer chosen is unlinked here.
    fn reconcile(&mut self, core: &pw::core::CoreRc) {
        let wanted = self.wanted();
        self.links.retain(|(out, input), _| wanted.iter().any(|(_, o, i)| o == out && i == input));
        let Some(me) = self.me else { return };
        for (node, out, input) in wanted {
            if self.links.contains_key(&(out, input)) {
                continue;
            }
            let props = pw::properties::properties! {
                "link.output.node" => node.to_string(),
                "link.output.port" => out.to_string(),
                "link.input.node" => me.to_string(),
                "link.input.port" => input.to_string(),
                "object.linger" => "false",
            };
            match core.create_object::<pw::link::Link>("link-factory", &props) {
                Ok(link) => {
                    self.links.insert((out, input), link);
                }
                Err(e) => tracing::warn!("linking an app's audio: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A graph with our input (FL, FR) as node 1, ports 10 and 11.
    fn graph(filter: Filter) -> Graph {
        let mut g = Graph { me: Some(1), filter, ..Default::default() };
        g.ports.insert(10, Port { node: 1, output: false, channel: "FL".into() });
        g.ports.insert(11, Port { node: 1, output: false, channel: "FR".into() });
        g
    }

    /// Adds an app stream with stereo output ports `node*10` and `node*10+1`.
    fn play(g: &mut Graph, node: u32, binary: &str, name: &str) -> bool {
        let included = g.add_stream(node, |k| match k {
            "application.process.binary" => Some(binary.to_owned()),
            "application.name" => Some(name.to_owned()),
            "node.name" => Some(format!("{name}-{node}")),
            _ => None,
        });
        g.ports.insert(node * 10, Port { node, output: true, channel: "FL".into() });
        g.ports.insert(node * 10 + 1, Port { node, output: true, channel: "FR".into() });
        included
    }

    fn linked_nodes(g: &Graph) -> Vec<u32> {
        let mut nodes: Vec<u32> = g.wanted().iter().map(|(n, _, _)| *n).collect();
        nodes.sort_unstable();
        nodes.dedup();
        nodes
    }

    #[test]
    fn sem_escolha_nenhum_som_sai() {
        let mut g = graph(Filter::Only(None));
        assert!(!play(&mut g, 5, "firefox", "Firefox"));
        assert!(!play(&mut g, 6, "spotify", "Spotify"));
        assert_eq!(linked_nodes(&g), [] as [u32; 0]);
    }

    #[test]
    fn so_o_app_escolhido_entra() {
        let mut g = graph(Filter::Only(Some("firefox".into())));
        assert!(play(&mut g, 5, "firefox", "Firefox"));
        assert!(!play(&mut g, 6, "spotify", "Spotify"));
        assert_eq!(linked_nodes(&g), [5]);
        // Both channels, to the matching side.
        let mut links = g.wanted();
        links.sort_unstable();
        assert_eq!(links, [(5, 50, 10), (5, 51, 11)]);
    }

    #[test]
    fn stream_que_some_e_volta_religa_sozinho() {
        let mut g = graph(Filter::Only(Some("firefox".into())));
        play(&mut g, 5, "firefox", "Firefox");
        g.forget(5);
        g.forget(50);
        g.forget(51);
        assert_eq!(linked_nodes(&g), [] as [u32; 0]);
        // The next song: a new node, even from another process of the same app.
        assert!(play(&mut g, 9, "firefox", "Firefox"));
        assert_eq!(linked_nodes(&g), [9]);
    }

    #[test]
    fn varios_streams_do_mesmo_app_entram_juntos() {
        let mut g = graph(Filter::Only(Some("chrome".into())));
        play(&mut g, 5, "chrome", "Google Chrome");
        play(&mut g, 6, "chrome", "Google Chrome");
        play(&mut g, 7, "mpv", "mpv");
        assert_eq!(linked_nodes(&g), [5, 6]);
    }

    #[test]
    fn trocar_a_escolha_troca_os_links() {
        let mut g = graph(Filter::Only(Some("firefox".into())));
        play(&mut g, 5, "firefox", "Firefox");
        play(&mut g, 6, "spotify", "Spotify");
        g.filter = Filter::Only(Some("spotify".into()));
        assert_eq!(linked_nodes(&g), [6]);
        g.filter = Filter::Only(None);
        assert_eq!(linked_nodes(&g), [] as [u32; 0]);
    }

    #[test]
    fn monitor_inteiro_inclui_tudo_menos_o_discord() {
        let mut g = graph(Filter::AllButDiscord);
        assert!(play(&mut g, 5, "firefox", "Firefox"));
        assert!(!play(&mut g, 6, "Discord", "Discord"));
        assert!(!play(&mut g, 7, "electron", "Vesktop"));
        assert_eq!(linked_nodes(&g), [5]);
    }

    #[test]
    fn discord_nao_entra_mesmo_escolhido() {
        let mut g = graph(Filter::Only(Some("discord".into())));
        assert!(!play(&mut g, 6, "discord", "Discord"));
        assert_eq!(linked_nodes(&g), [] as [u32; 0]);
    }

    #[test]
    fn avisos_do_proprio_telinha_nao_entram() {
        let mut g = graph(Filter::AllButDiscord);
        let name = chime_node_name();
        assert!(!g.add_stream(8, |k| (k == "node.name").then(|| name.clone())));
        assert_eq!(linked_nodes(&g), [] as [u32; 0]);
    }

    #[test]
    fn lista_viva_so_avisa_quando_muda() {
        let mut g = graph(Filter::Only(None));
        play(&mut g, 5, "firefox", "Firefox");
        let first = g.list_if_changed().expect("first list");
        assert_eq!(first, [AudioApp { id: "firefox".into(), label: "Firefox".into() }]);
        assert_eq!(g.list_if_changed(), None, "unchanged");
        // Another stream of the same app is not news.
        play(&mut g, 6, "firefox", "Firefox");
        assert_eq!(g.list_if_changed(), None);
        // Discord is not offered.
        play(&mut g, 7, "discord", "Discord");
        assert_eq!(g.list_if_changed(), None);
        play(&mut g, 8, "mpv", "mpv");
        assert_eq!(g.list_if_changed().map(|l| l.len()), Some(2));
    }
}
