//! Contract between the UI and the streaming engine. The UI sends
//! `Command`s and receives `Event`s; the engine runs in the background.

pub mod gate;
pub mod mock;
pub mod peer;
pub mod rtp;
pub mod session;
pub mod signaling;
pub mod turbo;

use crate::config::Quality;

/// Someone watching (the dots below the bubble).
#[derive(Debug, Clone, PartialEq)]
pub struct Viewer {
    pub name: String,
    /// Receives the native video (browser with WebCodecs). Otherwise shown dimmed.
    pub supported: bool,
}

#[derive(Debug, Clone)]
pub enum Command {
    /// Opens a new channel (this app becomes the host).
    Create { name: String, server: Option<String> },
    /// Joins a channel by number or invite link.
    Join { input: String, name: String, server: Option<String> },
    /// `discord`: Discord login session when only people in the call may
    /// watch (gatekeeper); `None` leaves it open to anyone with the link.
    StartLive { quality: Quality, discord: Option<String> },
    StopLive,
    SetQuality { quality: Quality },
    Leave,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EncoderInfo {
    /// Human-readable name: "NVENC", "VAAPI", "OpenH264".
    pub name: String,
    pub hardware: bool,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub enum Event {
    Connecting,
    Joined { code: String, invite: String, server: String },
    Failed(String),
    Viewers(Vec<Viewer>),
    Live(bool),
    Encoder(EncoderInfo),
    /// Size of the screen to be captured (for the resolution options).
    Screen { width: u32, height: u32 },
    Notice(String),
    /// Positive notice (not a problem).
    Info(String),
    Left,
}

/// Starts the engine (or the mock one, with TELINHA_MOCK=1) in the background.
/// Each event goes to `on_event`; commands come in through the returned channel.
/// Must be called inside tokio.
pub fn spawn(on_event: impl Fn(Event) + Send + 'static) -> tokio::sync::mpsc::Sender<Command> {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(32);
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::channel(64);
    if std::env::var_os("TELINHA_MOCK").is_some() {
        tokio::spawn(mock::run(cmd_rx, ev_tx));
    } else {
        tokio::spawn(session::run(cmd_rx, ev_tx));
    }
    tokio::spawn(async move {
        while let Some(e) = ev_rx.recv().await {
            on_event(e);
        }
    });
    cmd_tx
}
