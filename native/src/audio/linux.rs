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
//! TELINHA_SOM_TUDO=1 goes back to the old way: the whole default output.

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

pub fn start(tx: SyncSender<Vec<f32>>, stop: Arc<AtomicBool>) -> Result<String, String> {
    let everything = std::env::var_os("TELINHA_SOM_TUDO").is_some();
    std::thread::Builder::new()
        .name("telinha-pw-som".into())
        .spawn(move || {
            if let Err(e) = run(tx, stop, everything) {
                tracing::error!("PipeWire audio: {e}");
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(if everything { "saída de som padrão (PipeWire)" } else { "todo o som do computador menos o Discord (PipeWire)" }.into())
}

fn run(tx: SyncSender<Vec<f32>>, stop: Arc<AtomicBool>, everything: bool) -> Result<(), String> {
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
    let graph = Rc::new(RefCell::new(Graph::default()));
    let _registry_listener = (!everything).then(|| {
        let (g_add, g_rm, core) = (graph.clone(), graph.clone(), core.clone());
        registry
            .add_listener_local()
            .global(move |obj| {
                let mut g = g_add.borrow_mut();
                if g.learn(obj) {
                    g.reconcile(&core);
                }
            })
            .global_remove(move |id| g_rm.borrow_mut().forget(id))
            .register()
    });

    let ml = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop.load(Ordering::Relaxed) {
            ml.quit();
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

#[derive(Default)]
struct Graph {
    /// Our capture node.
    me: Option<u32>,
    /// App audio outputs: id → included in the stream?
    apps: HashMap<u32, bool>,
    ports: HashMap<u32, Port>,
    /// Links we created (app output, our input). They live as long as the proxy lives.
    links: HashMap<(u32, u32), pw::link::Link>,
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
                    // Our own chimes stay out (viewers already play theirs).
                    if props.get("node.name") == Some(chime_node_name().as_str()) {
                        tracing::info!("audio left out (Telinha's own chimes)");
                        self.apps.insert(obj.id, false);
                        return false;
                    }
                    let names = ["application.process.binary", "application.name", "application.id", "node.name"].map(|k| props.get(k).unwrap_or("").to_lowercase());
                    let discord = names.iter().any(|n| DISCORD.iter().any(|d| n.contains(d)));
                    let who = props.get("application.name").or(props.get("node.name")).unwrap_or("?");
                    tracing::info!(app = who, "{}", if discord { "audio left out (Discord)" } else { "audio included in the stream" });
                    self.apps.insert(obj.id, !discord);
                    return !discord;
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

    fn forget(&mut self, id: u32) {
        if self.me == Some(id) {
            self.me = None;
        }
        self.apps.remove(&id);
        self.ports.remove(&id);
        self.links.retain(|(out, inp), _| *out != id && *inp != id);
    }

    /// Links every allowed app output to our input, channel by channel
    /// (mono and center go to both sides).
    fn reconcile(&mut self, core: &pw::core::CoreRc) {
        let Some(me) = self.me else { return };
        let ours: HashMap<&str, u32> = self.ports.iter().filter(|(_, p)| p.node == me && !p.output).map(|(id, p)| (p.channel.as_str(), *id)).collect();
        if ours.is_empty() {
            return;
        }
        let mut wanted = Vec::new();
        for (id, p) in &self.ports {
            if !p.output || self.apps.get(&p.node) != Some(&true) {
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
