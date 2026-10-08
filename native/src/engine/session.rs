//! The channel session: talks to the signaling server, keeps the list of
//! who is in the channel, opens connections to viewers and handles the
//! stream. Runs as a single task, receiving everything through messages.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use webrtc::data_channel::DataChannel;
use webrtc::peer_connection::{PeerConnection, RTCIceCandidateInit, RTCIceServer, RTCPeerConnectionState, RTCSessionDescription};

use super::peer::{self, PeerEvent, Which};
use super::signaling::{self, Signal, Signaling};
use super::rtp;
use super::turbo::{self, KeyLimiter};
use super::{Command, Event};
use crate::config::{Priority, Quality};
use super::Viewer;
use crate::video::{self, EncodedFrame, Settings};

const PREFIX: &str = "telinha-canal-v1-";

#[derive(Debug, Clone, PartialEq)]
struct Member {
    id: String,
    name: String,
    sharing: bool,
    /// Receives video via WebCodecs (Chrome/Edge). Only these receive from the app.
    wc: bool,
    /// Streams only to people in the Discord call (requires a pass).
    discord: bool,
    /// Discord avatar URL (only from Discord's CDN), passed along in the roster.
    avatar: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// I am a guest: connection to the host.
    HostLink,
    /// I am the host: connection from a guest.
    Guest,
    /// I stream: video over the data channel (WebCodecs, minimal latency).
    Turbo,
    /// I stream: plain RTP video (relay, bad connection, Firefox/Safari).
    Media,
    /// I stream: someone came to hand over the Discord pass.
    Pass,
}

struct Conn {
    peer: String,
    kind: Kind,
    pc: Arc<dyn PeerConnection>,
    control: Option<Arc<dyn DataChannel>>,
    video: Option<Arc<dyn DataChannel>>,
    remote_set: bool,
    pending: Vec<RTCIceCandidateInit>,
    open: bool,
    /// Name and capability sent in the offer (guest of the host).
    meta: Value,
    sender: Option<turbo::Sender>,
    track: Option<Arc<webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample>>,
    rtp_sender: Option<rtp::Sender>,
    audio_track: Option<Arc<webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample>>,
    audio_sender: Option<rtp::AudioSender>,
    /// Audio-only call (goes with the data channel video).
    audio_only: bool,
    health: Health,
}

#[derive(Default)]
struct Health {
    last: Option<(u64, u64, u64)>,
    bad: u32,
    good: u32,
}

struct Live {
    audio: Option<crate::audio::Audio>,
    control: Arc<video::Control>,
    frames: broadcast::Sender<Arc<EncodedFrame>>,
    quality: Quality,
    /// Current bitrate (adaptation lowers it when someone receives poorly).
    bitrate: Arc<AtomicU32>,
    keys: Arc<KeyLimiter>,
    epoch: Instant,
    last_captured: u64,
    last_tick: Instant,
    /// Rates for the log every 10 s: when, and how many captured/encoded frames then.
    log_at: (Instant, u64, u64),
    capture_fps: f32,
}

enum Role {
    Host,
    Guest { host: String, link: String },
}

struct Session {
    ui: mpsc::Sender<Event>,
    sig: Signaling,
    peers: mpsc::Sender<PeerEvent>,
    ice: Vec<RTCIceServer>,
    server: String,
    me: String,
    name: String,
    code: String,
    role: Role,
    joined: bool,
    members: Vec<Member>,
    conns: HashMap<String, Conn>,
    live: Option<Live>,
    quality: Quality,
    inputs: Option<mpsc::Receiver<Input>>,
    /// Who receives via RTP after doing poorly on the data channel (or via relay).
    demoted: std::collections::HashSet<String>,
    /// Discord session for the gatekeeper of the next stream.
    discord: Option<String>,
    /// Gatekeeper of the current stream (only people in the call can watch).
    gate: Option<super::gate::Gate>,
    /// Who was in the channel at the last roster (for the join and leave
    /// chimes). None until the first roster: that one is not news.
    heard: Option<std::collections::HashSet<String>>,
}

pub async fn run(mut cmds: mpsc::Receiver<Command>, ui: mpsc::Sender<Event>) {
    // Outside a channel, just wait for a command to join or open one.
    while let Some(cmd) = cmds.recv().await {
        let started = match cmd {
            Command::Create { name, server } => start(&ui, name, server, None).await,
            Command::Join { input, name, server } => match parse_invite(&input, server) {
                Ok((server, code)) => start(&ui, name, Some(server), Some(code)).await,
                Err(e) => Err(e),
            },
            _ => continue,
        };
        match started {
            Ok(mut s) => {
                s.serve(&mut cmds).await;
                s.shutdown().await;
                let _ = ui.send(Event::Left).await;
            }
            Err(e) => {
                let _ = ui.send(Event::Failed(e)).await;
            }
        }
    }
}

/// Accepts "4821", "https://server/#4821" or "server/#4821".
fn parse_invite(input: &str, server: Option<String>) -> Result<(String, String), String> {
    let input = input.trim();
    let digits: String = input.rsplit('#').next().unwrap_or(input).chars().filter(char::is_ascii_digit).collect();
    if digits.len() != 4 {
        return Err("Digite os 4 números do canal ou cole o link de convite.".into());
    }
    if input.contains('#') || input.contains('/') {
        let host = input.trim_start_matches("https://").trim_start_matches("http://").split(['/', '#', '?']).next().unwrap_or_default();
        if !host.is_empty() {
            return Ok((host.to_owned(), digits));
        }
    }
    server.map(|s| (s, digits)).ok_or_else(|| "Cole o link de convite inteiro na primeira vez, para o app saber qual servidor usar.".into())
}

async fn start(ui: &mpsc::Sender<Event>, name: String, server: Option<String>, code: Option<String>) -> Result<Session, String> {
    let _ = ui.send(Event::Connecting).await;
    let server = server.ok_or("Entre num canal pelo link de convite uma vez, para o app saber qual servidor usar.")?;
    let ice = signaling::ice_servers(&server).await?;
    let (sig_tx, mut sig_rx) = mpsc::channel(256);
    let (peer_tx, peer_rx) = mpsc::channel(1024);

    // Host registers the channel id (tries another number if it is taken).
    let (me, code, sig) = match code {
        Some(code) => {
            let me = format!("telinha-app-{:012x}", rand::random::<u64>() & 0xffff_ffff_ffff);
            let sig = Signaling::connect(&server, &me, sig_tx.clone()).await?;
            wait_open(&mut sig_rx).await?;
            (me, code, sig)
        }
        None => {
            let mut attempt = 0;
            loop {
                let code = rand::random_range(1000..10000u32).to_string();
                let me = format!("{PREFIX}{code}");
                let sig = Signaling::connect(&server, &me, sig_tx.clone()).await?;
                match wait_open(&mut sig_rx).await {
                    Ok(()) => break (me, code, sig),
                    Err(_) if attempt < 5 => attempt += 1,
                    Err(e) => return Err(e),
                }
            }
        }
    };

    let host_id = format!("{PREFIX}{code}");
    let is_host = me == host_id;
    let mut s = Session {
        ui: ui.clone(),
        sig,
        peers: peer_tx,
        ice,
        server: server.clone(),
        me: me.clone(),
        name: name.clone(),
        code: code.clone(),
        role: Role::Host,
        joined: false,
        members: vec![],
        conns: HashMap::new(),
        live: None,
        quality: Quality::default(),
        inputs: None,
        demoted: Default::default(),
        discord: None,
        gate: None,
        heard: None,
    };
    s.spawn_signal_forwarder(sig_rx, peer_rx);

    if is_host {
        s.heard = Some([me.clone()].into());
        s.members = vec![Member { id: me, name, sharing: false, wc: false, discord: false, avatar: String::new() }];
        s.joined = true;
        s.announce_joined().await;
    } else {
        let link = s.open_data(&host_id, Kind::HostLink, json!({ "name": s.name, "wc": false })).await?;
        s.role = Role::Guest { host: host_id, link };
    }
    Ok(s)
}

async fn wait_open(rx: &mut mpsc::Receiver<Signal>) -> Result<(), String> {
    let deadline = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            s = rx.recv() => match s {
                Some(Signal::Open) => return Ok(()),
                Some(Signal::IdTaken) => return Err("esse número de canal já está em uso".into()),
                Some(Signal::Error(e)) => return Err(e),
                Some(_) => {}
                None => return Err("o servidor fechou a conexão".into()),
            },
            _ = &mut deadline => return Err("o servidor demorou demais para responder".into()),
        }
    }
}

enum Input {
    Signal(Signal),
    Peer(PeerEvent),
}

impl Session {
    /// Merges signaling and connection events into a single channel (`inputs`).
    fn spawn_signal_forwarder(&mut self, mut sig_rx: mpsc::Receiver<Signal>, mut peer_rx: mpsc::Receiver<PeerEvent>) {
        let (tx, rx) = mpsc::channel(1024);
        let t2 = tx.clone();
        tokio::spawn(async move {
            while let Some(s) = sig_rx.recv().await {
                if tx.send(Input::Signal(s)).await.is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            while let Some(p) = peer_rx.recv().await {
                if t2.send(Input::Peer(p)).await.is_err() {
                    break;
                }
            }
        });
        self.inputs = Some(rx);
    }
}

impl Session {
    async fn serve(&mut self, cmds: &mut mpsc::Receiver<Command>) {
        let mut inputs = self.inputs.take().expect("session inputs");
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let join_deadline = Instant::now() + Duration::from_secs(15);
        loop {
            tokio::select! {
                cmd = cmds.recv() => match cmd {
                    Some(Command::Leave) | None => return,
                    Some(c) => self.on_command(c).await,
                },
                input = inputs.recv() => match input {
                    Some(Input::Signal(Signal::Closed)) => {
                        let _ = self.ui.send(Event::Failed("A conexão com o servidor caiu.".into())).await;
                        return;
                    }
                    Some(Input::Signal(s)) => if self.on_signal(s).await.is_err() { return },
                    Some(Input::Peer(p)) => if self.on_peer(p).await.is_err() { return },
                    None => return,
                },
                _ = tick.tick() => {
                    if !self.joined && Instant::now() > join_deadline {
                        let _ = self.ui.send(Event::Failed(format!("O canal {} não respondeu. Confira o número com quem te convidou.", self.code))).await;
                        return;
                    }
                    self.on_tick().await;
                }
            }
        }
    }

    async fn announce_joined(&self) {
        let _ = self
            .ui
            .send(Event::Joined { code: self.code.clone(), invite: format!("https://{}/#{}", self.server, self.code), server: self.server.clone() })
            .await;
        self.publish_viewers().await;
    }

    async fn publish_viewers(&self) {
        let viewers = self
            .members
            .iter()
            .filter(|m| m.id != self.me)
            // With the gatekeeper, only those who showed a pass count (with their Discord name).
            .filter_map(|m| match &self.gate {
                Some(g) => g.allowed.get(&m.id).map(|(_, name)| Viewer { name: name.clone(), supported: true }),
                // Those without WebCodecs receive via RTP: everyone can watch.
                None => Some(Viewer { name: m.name.clone(), supported: true }),
            })
            .collect();
        let _ = self.ui.send(Event::Viewers(viewers)).await;
    }

    /* ---------------- conexões ---------------- */

    /// Opens a PeerJS-style data connection (I make the offer).
    async fn open_data(&mut self, dst: &str, kind: Kind, metadata: Value) -> Result<String, String> {
        let conn = format!("dc_{:012x}", rand::random::<u64>() & 0xffff_ffff_ffff);
        let pc = peer::new_connection(&self.ice, &conn, false, self.peers.clone()).await?;
        let control = peer::control_channel(&pc, &conn).await?;
        peer::pump(control.clone(), conn.clone(), Which::Control, self.peers.clone());
        let video = if kind == Kind::Turbo {
            let v = peer::video_channel(&pc).await?;
            peer::pump(v.clone(), conn.clone(), Which::Video, self.peers.clone());
            Some(v)
        } else {
            None
        };
        let offer = pc.create_offer(None).await.map_err(|e| e.to_string())?;
        pc.set_local_description(offer.clone()).await.map_err(|e| e.to_string())?;
        self.sig.send(
            "OFFER",
            dst,
            json!({
                "sdp": { "type": "offer", "sdp": offer.sdp },
                "type": "data",
                "connectionId": conn,
                "label": conn,
                "reliable": true,
                "serialization": "json",
                "metadata": metadata,
            }),
        );
        self.conns.insert(
            conn.clone(),
            Conn {
                peer: dst.to_owned(),
                kind,
                pc,
                control: Some(control),
                video,
                remote_set: false,
                pending: vec![],
                open: false,
                meta: Value::Null,
                sender: None,
                track: None,
                rtp_sender: None,
                audio_track: None,
                audio_sender: None,
                audio_only: false,
                health: Health::default(),
            },
        );
        Ok(conn)
    }

    /// PeerJS media call (I make the offer): hardware-encoded video over RTP plus
    /// the computer's audio, or audio only (`audio_only`, for those receiving
    /// video over the data channel).
    async fn open_media(&mut self, dst: &str, audio_only: bool) -> Result<String, String> {
        let conn = format!("mc_{:012x}", rand::random::<u64>() & 0xffff_ffff_ffff);
        let pc = peer::new_connection(&self.ice, &conn, true, self.peers.clone()).await?;
        let track = if audio_only { None } else { Some(rtp::add_video_track(&pc).await?) };
        let has_audio = self.live.as_ref().is_some_and(|l| l.audio.is_some());
        let audio_track = if has_audio { Some(rtp::add_audio_track(&pc).await?) } else { None };
        let offer = pc.create_offer(None).await.map_err(|e| e.to_string())?;
        pc.set_local_description(offer.clone()).await.map_err(|e| e.to_string())?;
        self.sig.send(
            "OFFER",
            dst,
            json!({
                "sdp": { "type": "offer", "sdp": offer.sdp },
                "type": "media",
                "connectionId": conn,
                "metadata": if audio_only { json!({ "name": self.name, "audioOnly": true }) } else { json!({ "name": self.name, "codec": "video/H264" }) },
            }),
        );
        self.conns.insert(
            conn.clone(),
            Conn {
                peer: dst.to_owned(),
                kind: Kind::Media,
                pc,
                control: None,
                video: None,
                remote_set: false,
                pending: vec![],
                open: false,
                meta: Value::Null,
                sender: None,
                track,
                rtp_sender: None,
                audio_track,
                audio_sender: None,
                audio_only,
                health: Health::default(),
            },
        );
        Ok(conn)
    }

    /// Moves someone off the data channel to RTP.
    async fn demote(&mut self, conn: &str, why: &str) {
        let Some(peer) = self.conns.get(conn).map(|c| c.peer.clone()) else { return };
        tracing::info!(peer, why, "switching to RTP video");
        self.demoted.insert(peer.clone());
        self.close_conn(conn).await;
        // Their audio-only call becomes useless: the RTP one carries video and audio together.
        let audio_calls: Vec<String> = self.conns.iter().filter(|(_, c)| c.peer == peer && c.audio_only).map(|(k, _)| k.clone()).collect();
        for c in audio_calls {
            self.close_conn(&c).await;
        }
        if self.live.is_some() {
            if let Err(e) = self.open_media(&peer, false).await {
                tracing::warn!("failed to open RTP video for {peer}: {e}");
            }
        }
    }

    async fn close_conn(&mut self, id: &str) {
        if let Some(c) = self.conns.remove(id) {
            if let Some(ch) = &c.control {
                peer::send_json(ch, &json!({ "__peerData": { "type": "close" } })).await;
            }
            let _ = c.pc.close().await;
        }
    }

    async fn shutdown(&mut self) {
        self.stop_live().await;
        let ids: Vec<String> = self.conns.keys().cloned().collect();
        for id in ids {
            self.close_conn(&id).await;
        }
    }

    /* ---------------- sinalização ---------------- */

    async fn on_signal(&mut self, s: Signal) -> Result<(), ()> {
        match s {
            Signal::Offer { src, payload } => self.on_offer(src, payload).await,
            Signal::Answer { payload, .. } => {
                let conn = payload["connectionId"].as_str().unwrap_or_default().to_owned();
                let sdp = payload["sdp"]["sdp"].as_str().unwrap_or_default().to_owned();
                if let Some(c) = self.conns.get_mut(&conn) {
                    if let Ok(desc) = RTCSessionDescription::answer(sdp) {
                        if c.pc.set_remote_description(desc).await.is_ok() {
                            c.remote_set = true;
                            for cand in c.pending.drain(..) {
                                let _ = c.pc.add_ice_candidate(cand).await;
                            }
                        }
                    }
                }
            }
            Signal::Candidate { payload, .. } => {
                let conn = payload["connectionId"].as_str().unwrap_or_default().to_owned();
                if let (Some(c), Some(cand)) = (self.conns.get_mut(&conn), peer::parse_candidate(&payload)) {
                    if c.remote_set {
                        let _ = c.pc.add_ice_candidate(cand).await;
                    } else {
                        c.pending.push(cand);
                    }
                }
            }
            Signal::Gone { peer } => {
                if let Role::Guest { host, .. } = &self.role {
                    if *host == peer {
                        let msg = if self.joined { "O canal saiu do ar.".into() } else { format!("O canal {} não está no ar. Confira o número com quem te convidou.", self.code) };
                        let _ = self.ui.send(Event::Failed(msg)).await;
                        return Err(());
                    }
                }
                let gone: Vec<String> = self.conns.iter().filter(|(_, c)| c.peer == peer).map(|(k, _)| k.clone()).collect();
                for id in gone {
                    self.on_conn_closed(&id).await;
                }
            }
            Signal::Error(e) => tracing::warn!("server: {e}"),
            _ => {}
        }
        Ok(())
    }

    async fn on_offer(&mut self, src: String, payload: Value) {
        // The host accepts guests joining the channel; a streamer with the
        // gatekeeper accepts pass deliveries. The app does not yet watch
        // other people's video.
        let pass = payload["metadata"]["kind"] == "passe";
        if payload["type"] != "data" || payload["metadata"]["kind"] == "turbo" {
            return;
        }
        if (pass && self.gate.is_none()) || (!pass && !matches!(self.role, Role::Host)) {
            return;
        }
        let mut meta = payload["metadata"].clone();
        if pass {
            let gate = self.gate.as_mut().expect("checked above");
            let verdict = gate.check(meta["passe"].as_str().unwrap_or_default(), &self.code, &self.me);
            meta = match verdict {
                Ok((user, name)) => {
                    tracing::info!(who = %name, "pass accepted");
                    gate.admit(src.clone(), user, name);
                    json!({ "ok": true })
                }
                Err(why) => {
                    tracing::info!("pass rejected: {why}");
                    // Bot has not confirmed yet: the viewer retries shortly.
                    json!({ "ok": false, "motivo": why, "tentar": !gate.ready() })
                }
            };
        }
        let conn = payload["connectionId"].as_str().unwrap_or_default().to_owned();
        let sdp = payload["sdp"]["sdp"].as_str().unwrap_or_default().to_owned();
        let Ok(pc) = peer::new_connection(&self.ice, &conn, false, self.peers.clone()).await else { return };
        let Ok(desc) = RTCSessionDescription::offer(sdp) else { return };
        if pc.set_remote_description(desc).await.is_err() {
            return;
        }
        let Ok(answer) = pc.create_answer(None).await else { return };
        if pc.set_local_description(answer.clone()).await.is_err() {
            return;
        }
        self.sig.send("ANSWER", &src, json!({ "sdp": { "type": "answer", "sdp": answer.sdp }, "type": "data", "connectionId": conn }));
        self.conns.insert(
            conn,
            Conn {
                peer: src,
                kind: if pass { Kind::Pass } else { Kind::Guest },
                pc,
                control: None,
                video: None,
                remote_set: true,
                pending: vec![],
                open: false,
                meta,
                sender: None,
                track: None,
                rtp_sender: None,
                audio_track: None,
                audio_sender: None,
                audio_only: false,
                health: Health::default(),
            },
        );
        if pass {
            self.publish_viewers().await;
            self.sync_viewers().await;
        }
    }

    /* ---------------- conexões: eventos ---------------- */

    async fn on_peer(&mut self, e: PeerEvent) -> Result<(), ()> {
        match e {
            PeerEvent::LocalCandidate { conn, candidate } => {
                if let Some(c) = self.conns.get(&conn) {
                    let kind = if c.kind == Kind::Media { "media" } else { "data" };
                    self.sig.send("CANDIDATE", &c.peer, peer::candidate_payload(&candidate, kind, &conn));
                }
            }
            PeerEvent::RemoteChannel { conn, channel } => {
                if let Some(c) = self.conns.get_mut(&conn) {
                    peer::pump(channel.clone(), conn.clone(), Which::Control, self.peers.clone());
                    c.control = Some(channel);
                }
            }
            PeerEvent::Open { conn, which } => return self.on_open(conn, which).await,
            PeerEvent::Message { conn, which: Which::Control, data } => {
                if let Ok(v) = serde_json::from_slice::<Value>(&data) {
                    return self.on_message(conn, v).await;
                }
            }
            PeerEvent::Message { .. } => {}
            PeerEvent::Closed { conn, which: Which::Control } => self.on_conn_closed(&conn).await,
            PeerEvent::Closed { .. } => {}
            PeerEvent::State { conn, state } => {
                if matches!(state, RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed) {
                    self.on_conn_closed(&conn).await;
                } else if state == RTCPeerConnectionState::Connected {
                    // RTP video connected: start sending from a keyframe.
                    if let (Some(live), Some(c)) = (&self.live, self.conns.get_mut(&conn)) {
                        if c.kind == Kind::Media && !c.open {
                            c.open = true;
                            if let Some(track) = c.track.clone() {
                                c.rtp_sender = Some(rtp::spawn(track, live.frames.subscribe(), live.control.clone(), live.keys.clone(), live.quality.fps));
                            }
                            if let (Some(track), Some(audio)) = (c.audio_track.clone(), &live.audio) {
                                c.audio_sender = Some(rtp::spawn_audio(track, audio.packets.subscribe()));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn on_open(&mut self, conn: String, which: Which) -> Result<(), ()> {
        let Some(c) = self.conns.get_mut(&conn) else { return Ok(()) };
        match (c.kind, which) {
            (Kind::HostLink, Which::Control) => {
                c.open = true;
                self.joined = true;
                self.announce_joined().await;
            }
            (Kind::Guest, Which::Control) => {
                c.open = true;
                let name = c.meta["name"].as_str().unwrap_or("Alguém").chars().take(24).collect();
                let wc = c.meta["wc"].as_bool().unwrap_or(false);
                let id = c.peer.clone();
                self.members.retain(|m| m.id != id);
                let avatar = c.meta["avatar"].as_str().filter(|a| a.starts_with("https://cdn.discordapp.com/avatars/")).unwrap_or_default().to_owned();
                self.members.push(Member { id, name, sharing: false, wc, discord: false, avatar });
                self.broadcast_roster().await;
            }
            (Kind::Turbo, Which::Video) => {
                // Video channel open: start sending from a keyframe.
                if let (Some(live), Some(video)) = (&self.live, c.video.clone()) {
                    c.sender = Some(turbo::spawn(video, live.frames.subscribe(), live.control.clone(), live.keys.clone(), live.bitrate.clone(), live.epoch));
                }
            }
            (Kind::Turbo, Which::Control) => c.open = true,
            (Kind::Pass, Which::Control) => {
                // Tell whoever delivered the pass whether it worked (and why not).
                c.open = true;
                if let Some(ch) = c.control.clone() {
                    let mut msg = c.meta.clone();
                    msg["t"] = json!("passe");
                    peer::send_json(&ch, &msg).await;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn on_conn_closed(&mut self, conn: &str) {
        let Some(c) = self.conns.remove(conn) else { return };
        let _ = c.pc.close().await;
        match c.kind {
            Kind::HostLink => {
                let _ = self.ui.send(Event::Failed("O canal saiu do ar.".into())).await;
                // Without a host there is no channel: the session ends on the next command.
                self.members.clear();
                self.publish_viewers().await;
            }
            Kind::Guest => {
                self.members.retain(|m| m.id != c.peer);
                self.broadcast_roster().await;
            }
            Kind::Turbo | Kind::Media | Kind::Pass => {}
        }
    }

    async fn on_message(&mut self, conn: String, v: Value) -> Result<(), ()> {
        if v["__peerData"]["type"] == "close" {
            self.on_conn_closed(&conn).await;
            return Ok(());
        }
        let Some(kind) = self.conns.get(&conn).map(|c| c.kind) else { return Ok(()) };
        match (kind, v["t"].as_str().unwrap_or_default()) {
            (Kind::HostLink, "roster") => {
                let members: Vec<Member> = v["members"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|m| Member {
                                id: m["id"].as_str().unwrap_or_default().to_owned(),
                                name: m["name"].as_str().unwrap_or("Alguém").to_owned(),
                                sharing: m["sharing"].as_bool().unwrap_or(false),
                                wc: m["wc"].as_bool().unwrap_or(false),
                                discord: m["discord"].as_bool().unwrap_or(false),
                                avatar: m["avatar"].as_str().unwrap_or_default().to_owned(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.members = members;
                self.chime();
                self.publish_viewers().await;
                self.sync_viewers().await;
            }
            (Kind::Guest, "sharing") => {
                let peer = self.conns.get(&conn).map(|c| c.peer.clone()).unwrap_or_default();
                if let Some(m) = self.members.iter_mut().find(|m| m.id == peer) {
                    m.sharing = v["on"].as_bool().unwrap_or(false);
                    m.discord = m.sharing && v["discord"].as_bool().unwrap_or(false);
                }
                self.broadcast_roster().await;
            }
            (Kind::Turbo, "key") => {
                if let Some(live) = &self.live {
                    live.keys.request(&live.control, Duration::from_millis(500));
                }
            }
            (Kind::Turbo, "rx") => self.on_report(&conn, &v).await,
            (Kind::Turbo, "unsupported") => {
                // The browser cannot decode this video: stop sending.
                let peer = self.conns.get(&conn).map(|c| c.peer.clone()).unwrap_or_default();
                self.close_conn(&conn).await;
                if let Some(m) = self.members.iter_mut().find(|m| m.id == peer) {
                    m.wc = false;
                }
                self.publish_viewers().await;
            }
            _ => {}
        }
        Ok(())
    }

    /// Host sends the list of who is in the channel to everyone.
    async fn broadcast_roster(&mut self) {
        if let Some(me) = self.members.iter_mut().find(|m| m.id == self.me) {
            me.sharing = self.live.is_some();
            me.discord = self.gate.is_some();
        }
        let members: Vec<Value> =
            self.members.iter().map(|m| json!({ "id": m.id, "name": m.name, "avatar": m.avatar, "sharing": m.sharing, "wc": m.wc, "discord": m.discord })).collect();
        let msg = json!({ "t": "roster", "members": members });
        for c in self.conns.values().filter(|c| c.kind == Kind::Guest && c.open) {
            if let Some(ch) = &c.control {
                peer::send_json(ch, &msg).await;
            }
        }
        self.chime();
        self.publish_viewers().await;
        self.sync_viewers().await;
    }

    /// Plays the join or leave chime when someone else entered or left the channel.
    fn chime(&mut self) {
        let now: std::collections::HashSet<String> = self.members.iter().map(|m| m.id.clone()).collect();
        let Some(before) = self.heard.replace(now.clone()) else { return };
        if now.difference(&before).any(|id| *id != self.me) {
            crate::audio::chime::play(crate::audio::chime::Chime::Join);
        } else if before.difference(&now).any(|id| *id != self.me) {
            crate::audio::chime::play(crate::audio::chime::Chime::Leave);
        }
    }

    /* ---------------- transmissão ---------------- */

    async fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::StartLive { quality, discord } => {
                self.quality = quality;
                self.discord = discord;
                if let Err(e) = self.start_live().await {
                    let _ = self.ui.send(Event::Failed(e)).await;
                }
            }
            Command::StopLive => self.stop_live().await,
            Command::SetQuality { quality } => {
                let audio_changed = quality.system_audio != self.quality.system_audio;
                self.quality = quality;
                if audio_changed && self.live.is_some() {
                    // Toggling audio changes the tracks: restart the stream.
                    self.stop_live().await;
                    if let Err(e) = self.start_live().await {
                        let _ = self.ui.send(Event::Failed(e)).await;
                    }
                    return;
                }
                if let Some(live) = &mut self.live {
                    let native = live.control.stats().native.unwrap_or((1920, 1080));
                    let bitrate = quality.bitrate(native);
                    live.quality = quality;
                    live.bitrate.store(bitrate, Ordering::Relaxed);
                    live.control.reconfigure(Settings { quality, bitrate });
                }
            }
            _ => {}
        }
    }

    async fn start_live(&mut self) -> Result<(), String> {
        if self.live.is_some() {
            return Ok(());
        }
        let source = crate::capture::open(self.quality).await?;
        if let Some(session) = self.discord.clone() {
            self.gate = Some(super::gate::Gate::open(&self.server, session, &self.code, &self.me));
        }
        let audio = if self.quality.system_audio {
            crate::audio::start().map_err(|e| tracing::warn!("no computer audio: {e}")).ok()
        } else {
            None
        };
        let (frames, _) = broadcast::channel(8);
        // The real bitrate depends on the screen size, which capture reports on the
        // first frame; start with the 1080p one and correct it within the first second.
        let bitrate = self.quality.bitrate((1920, 1080));
        let control = video::start(source, Settings { quality: self.quality, bitrate }, frames.clone());
        self.live = Some(Live {
            audio,
            control,
            frames,
            quality: self.quality,
            bitrate: Arc::new(AtomicU32::new(bitrate)),
            keys: Arc::new(KeyLimiter::new()),
            epoch: Instant::now(),
            last_captured: 0,
            log_at: (Instant::now(), 0, 0),
            last_tick: Instant::now(),
            capture_fps: 0.0,
        });
        self.set_sharing(true).await;
        self.sync_viewers().await;
        let _ = self.ui.send(Event::Live(true)).await;
        Ok(())
    }

    async fn stop_live(&mut self) {
        let Some(live) = self.live.take() else { return };
        if let Some(gate) = self.gate.take() {
            gate.close();
        }
        live.control.stop.store(true, Ordering::Relaxed);
        let turbo: Vec<String> = self.conns.iter().filter(|(_, c)| matches!(c.kind, Kind::Turbo | Kind::Media)).map(|(k, _)| k.clone()).collect();
        for id in turbo {
            self.close_conn(&id).await;
        }
        self.set_sharing(false).await;
        let _ = self.ui.send(Event::Live(false)).await;
    }

    async fn set_sharing(&mut self, on: bool) {
        match &self.role {
            Role::Host => self.broadcast_roster().await,
            Role::Guest { link, .. } => {
                if let Some(ch) = self.conns.get(link).and_then(|c| c.control.clone()) {
                    peer::send_json(&ch, &json!({ "t": "sharing", "on": on, "discord": on && self.gate.is_some() })).await;
                }
            }
        }
    }

    /// Opens video for newcomers and closes it for those who left. Browsers with
    /// WebCodecs receive over the data channel (minimal latency); the rest, and
    /// those who already did poorly there, receive RTP.
    async fn sync_viewers(&mut self) {
        if self.live.is_none() {
            return;
        }
        let targets: Vec<(String, bool)> = self
            .members
            .iter()
            .filter(|m| m.id != self.me)
            .filter(|m| self.gate.as_ref().is_none_or(|g| g.allowed.contains_key(&m.id)))
            .map(|m| (m.id.clone(), m.wc && !self.demoted.contains(&m.id)))
            .collect();
        let existing: Vec<(String, String, Kind, bool)> = self
            .conns
            .iter()
            .filter(|(_, c)| matches!(c.kind, Kind::Turbo | Kind::Media))
            .map(|(k, c)| (k.clone(), c.peer.clone(), c.kind, c.audio_only))
            .collect();
        for (conn, peer, _, _) in &existing {
            if !targets.iter().any(|(id, _)| id == peer) {
                self.close_conn(conn).await;
            }
        }
        let has_audio = self.live.as_ref().is_some_and(|l| l.audio.is_some());
        for (id, turbo) in targets {
            let has = |kind: Kind, audio_only: bool| existing.iter().any(|(_, p, k, a)| *p == id && *k == kind && *a == audio_only);
            if turbo {
                if !has(Kind::Turbo, false) {
                    let meta = json!({ "kind": "turbo", "name": self.name });
                    if let Err(e) = self.open_data(&id, Kind::Turbo, meta).await {
                        tracing::warn!("failed to open video for {id}: {e}");
                    }
                }
                if has_audio && !has(Kind::Media, true) {
                    if let Err(e) = self.open_media(&id, true).await {
                        tracing::warn!("failed to open audio for {id}: {e}");
                    }
                }
            } else if !has(Kind::Media, false) {
                if let Err(e) = self.open_media(&id, false).await {
                    tracing::warn!("failed to open RTP video for {id}: {e}");
                }
            }
        }
    }

    /// Viewer report. If someone receives much less than was sent, the video
    /// bitrate (shared by everyone) drops; if everyone receives well for a
    /// while, it climbs back up to the ideal.
    async fn on_report(&mut self, conn: &str, v: &Value) {
        // Over a relay the data channel stalls (long round trip + loss):
        // this person gets RTP, which retransmits and tolerates loss.
        if v["relay"].as_bool() == Some(true) {
            self.demote(conn, "relay").await;
            return;
        }
        let Some(c) = self.conns.get_mut(conn) else { return };
        let Some(counters) = c.sender.as_ref().map(|s| s.counters.clone()) else { return };
        let now = (counters.bytes.load(Ordering::Relaxed), v["bytes"].as_u64().unwrap_or(0), v["lost"].as_u64().unwrap_or(0));
        let Some(prev) = c.health.last.replace(now) else { return };
        let sent = now.0.saturating_sub(prev.0);
        let got = now.1.saturating_sub(prev.1);
        let lost = now.2.saturating_sub(prev.2);
        let bad = (sent > 50_000 && got < sent * 8 / 10) || lost >= 2;
        if self.adapt(conn, bad) {
            self.demote(conn, "receiving poorly over the data channel").await;
        }
    }

    /// Adjusts the bitrate (shared by everyone) based on one connection's health.
    /// Returns `true` if the connection is still bad even at the floor.
    fn adapt(&mut self, conn: &str, bad: bool) -> bool {
        let Some(live) = &self.live else { return false };
        let Some(c) = self.conns.get_mut(conn) else { return false };
        if bad {
            c.health.bad += 1;
            c.health.good = 0;
        } else {
            c.health.good += 1;
            c.health.bad = 0;
        }
        let ideal = live.quality.bitrate(live.control.stats().native.unwrap_or((1920, 1080)));
        let current = live.bitrate.load(Ordering::Relaxed);
        // Sharpness mode lowers the bitrate less (prefers dropping frames).
        let floor = match live.quality.priority {
            Priority::Fluidez => 1_500_000,
            Priority::Nitidez => (ideal / 2).max(1_500_000),
        };
        let mut give_up = false;
        let next = if c.health.bad >= 2 {
            give_up = current <= floor && c.health.bad >= 4;
            if current > floor {
                c.health.bad = 0;
            }
            (current * 4 / 5).max(floor)
        } else if c.health.good >= 10 && current < ideal {
            c.health.good = 0;
            (current * 11 / 10).min(ideal)
        } else {
            current
        };
        if next != current {
            tracing::info!(from = current, to = next, "adjusting bitrate");
            live.bitrate.store(next, Ordering::Relaxed);
            live.control.reconfigure(Settings { quality: live.quality, bitrate: next });
        }
        give_up && c.kind == Kind::Turbo
    }

    async fn on_tick(&mut self) {
        // Discord gatekeeper: bot response and retries.
        if let Some(change) = match &mut self.gate {
            Some(g) => g.poll().await,
            None => None,
        } {
            use super::gate::Change;
            let event = match change {
                Change::Ready { announced: false, .. } => Event::Notice("O bot não conseguiu postar no chat da call. Usem /telinha lá.".into()),
                Change::Ready { after_failure: true, .. } => Event::Info("Achei sua call: o Assistir está no chat dela.".into()),
                Change::Ready { .. } => Event::Info("O Assistir está no chat da sua call.".into()),
                Change::Failed(e) => Event::Notice(format!("{e} Tento de novo a cada 15 segundos.")),
            };
            tracing::info!("gatekeeper: {event:?}");
            let _ = self.ui.send(event).await;
        }
        let Some(live) = &mut self.live else { return };
        let st = live.control.stats();
        if video::NVENC_DRIVER_TOO_OLD.swap(false, Ordering::Relaxed) {
            let using = st.encoder.as_ref().map_or("outro codificador".to_owned(), |e| e.name.clone());
            let msg = format!(
                "O driver da sua placa NVIDIA é antigo demais para o NVENC (precisa da versão {} ou mais nova). {}; até lá, a transmissão usa {using}.",
                video::NVENC_MIN_DRIVER,
                if cfg!(windows) { "Atualize pelo app da NVIDIA" } else { "Atualize pelo gerenciador de drivers do sistema" }
            );
            tracing::warn!("{msg}");
            let _ = self.ui.send(Event::Notice(msg)).await;
        }
        if let Some(e) = &st.error {
            let msg = format!("A transmissão parou: {e}");
            self.stop_live().await;
            let _ = self.ui.send(Event::Failed(msg)).await;
            return;
        }
        // Correct the ideal bitrate once the real screen size is known.
        if let Some(native) = st.native {
            let ideal = live.quality.bitrate(native);
            if live.last_captured == 0 && live.bitrate.load(Ordering::Relaxed) != ideal {
                live.bitrate.store(ideal, Ordering::Relaxed);
                live.control.reconfigure(Settings { quality: live.quality, bitrate: ideal });
                let _ = self.ui.send(Event::Screen { width: native.0, height: native.1 }).await;
            }
        }
        let dt = live.last_tick.elapsed().as_secs_f32().max(0.001);
        live.capture_fps = (st.captured - live.last_captured) as f32 / dt;
        live.last_captured = st.captured;
        live.last_tick = Instant::now();
        // Where frames are lost, in the log: the compositor (capture) or the encoder.
        let (at, captured, encoded) = live.log_at;
        let secs = at.elapsed().as_secs_f32();
        if secs >= 10.0 {
            tracing::info!(
                capture_fps = format!("{:.1}", (st.captured - captured) as f32 / secs),
                encode_fps = format!("{:.1}", (st.encoded - encoded) as f32 / secs),
                encode_ms = format!("{:.1}", st.encode_ms.unwrap_or(0.0)),
                target_fps = live.quality.fps,
                "video rates"
            );
            live.log_at = (Instant::now(), st.captured, st.encoded);
        }
        if let Some(info) = st.encoder.clone() {
            let _ = self.ui.send(Event::Encoder(info)).await;
        }
        // RTP: loss from receiver reports (more than ~5% is bad).
        let media: Vec<(String, bool)> = self
            .conns
            .iter()
            .filter_map(|(k, c)| c.rtp_sender.as_ref().map(|s| (k.clone(), s.counters.fraction_lost.load(Ordering::Relaxed) > 13)))
            .collect();
        for (conn, bad) in media {
            self.adapt(&conn, bad);
        }
        let Some(live) = &mut self.live else { return };
        // Tell viewers how the sender side is doing ("Lá na origem" panel).
        let fps = live.capture_fps;
        for c in self.conns.values().filter(|c| c.kind == Kind::Turbo && c.open) {
            let dropped = c.sender.as_ref().map_or(0, |s| s.counters.dropped.load(Ordering::Relaxed));
            let msg = json!({
                "t": "tx",
                "captured": st.captured,
                "skipped": st.skipped,
                "dropped": dropped,
                "hardware": st.encoder.as_ref().map(|e| e.hardware),
                "encodeMs": st.encode_ms,
                "captureFps": fps.round(),
                "hidden": false,
            });
            if let Some(ch) = &c.control {
                peer::send_json(ch, &msg).await;
            }
        }
    }
}
