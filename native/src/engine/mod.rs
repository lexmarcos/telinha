//! Contrato entre a interface e o motor de transmissão. A interface manda
//! `Command`s e recebe `Event`s; o motor roda em segundo plano.

pub mod gate;
pub mod mock;
pub mod peer;
pub mod rtp;
pub mod session;
pub mod signaling;
pub mod turbo;

use crate::config::Quality;

/// Alguém assistindo (os pontinhos embaixo da bolha).
#[derive(Debug, Clone, PartialEq)]
pub struct Viewer {
    pub name: String,
    /// Recebe o vídeo nativo (navegador com WebCodecs). Sem isso, aparece apagado.
    pub supported: bool,
}

#[derive(Debug, Clone)]
pub enum Command {
    /// Abre um canal novo (este app vira o anfitrião).
    Create { name: String, server: Option<String> },
    /// Entra num canal pelo número ou pelo link de convite.
    Join { input: String, name: String, server: Option<String> },
    /// `discord`: sessão do login com o Discord quando só quem está na call
    /// pode assistir (porteiro); `None` deixa aberto para quem tiver o link.
    StartLive { quality: Quality, discord: Option<String> },
    StopLive,
    SetQuality { quality: Quality },
    Leave,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EncoderInfo {
    /// Nome para gente: "NVENC", "VAAPI", "OpenH264".
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
    /// Tamanho da tela que vai ser capturada (para as opções de resolução).
    Screen { width: u32, height: u32 },
    Notice(String),
    /// Aviso bom (não é problema).
    Info(String),
    Left,
}

/// Liga o motor (ou o de mentira, com TELINHA_MOCK=1) em segundo plano.
/// Cada evento vai para `on_event`; os comandos entram pelo canal devolvido.
/// Precisa ser chamado dentro do tokio.
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
