//! Mock engine for developing and testing the UI: simulates opening and
//! joining a channel, people arriving, and the stream. Enabled with TELINHA_MOCK=1.
//! TELINHA_MOCK_FALHA makes one part fail: `servidor` (no known server), `conexao`
//! (opening a channel), `captura` (starting the stream) or `copia` (the clipboard,
//! which the app checks). TELINHA_MOCK_PORTAL=janela plays the Linux portal
//! answering with a window; `cancelar` cancelling. Sources and apps for the
//! panels of what to share and what to hear are made up here too.

use std::time::Duration;

use tokio::sync::mpsc;

use super::{AudioApp, CaptureTarget, Command, EncoderInfo, Event, Viewer};
use crate::capture::target::{DesktopOutput, MonitorEntry, SourceList, WindowEntry};

pub async fn run(mut rx: mpsc::Receiver<Command>, out: mpsc::Sender<Event>) {
    let send = async |e: Event| {
        let _ = out.send(e).await;
    };
    let fail = std::env::var("TELINHA_MOCK_FALHA").unwrap_or_default();
    // Like the real engine, what needs a channel is ignored outside one.
    let mut in_channel = false;
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Command::Create { .. } if fail == "servidor" => {
                send(Event::Connecting).await;
                send(Event::Failed("Entre num canal pelo link de convite uma vez, para o app saber qual servidor usar.".into())).await;
            }
            Command::Create { .. } if fail == "conexao" => {
                send(Event::Connecting).await;
                tokio::time::sleep(Duration::from_millis(900)).await;
                send(Event::Failed("Não consegui falar com o servidor do Telinha. Confira sua internet e tente de novo.".into())).await;
            }
            Command::Create { .. } | Command::Join { .. } => {
                send(Event::Connecting).await;
                tokio::time::sleep(Duration::from_millis(900)).await;
                in_channel = true;
                send(Event::Joined { code: "4821".into(), invite: "https://exemplo/#4821".into(), server: "exemplo".into() }).await;
                send(Event::Screen { width: 2560, height: 1440 }).await;
                send(Event::Viewers(demo_viewers())).await;
            }
            Command::StartLive { target, .. } if in_channel => {
                // The capture opening, or the system's picker.
                tokio::time::sleep(Duration::from_millis(1000)).await;
                if fail == "captura" {
                    send(Event::Failed("Não consegui abrir a captura da tela.".into())).await;
                    continue;
                }
                let portal = std::env::var("TELINHA_MOCK_PORTAL").unwrap_or_default();
                if portal == "cancelar" {
                    send(Event::CaptureCancelled).await;
                    continue;
                }
                let window = matches!(target, CaptureTarget::Window { .. }) || portal == "janela";
                let (width, height) = if window { (1281, 801) } else { (2560, 1440) };
                send(Event::Capturing { window }).await;
                send(Event::Screen { width, height }).await;
                send(Event::Encoder(EncoderInfo { name: "NVENC".into(), hardware: true, width: width & !1, height: height & !1 })).await;
                if portal == "janela" {
                    send(Event::AudioApps { apps: audio_apps(), selected: None, ask: true }).await;
                }
                send(Event::Live(true)).await;
            }
            Command::StopLive if in_channel => send(Event::Live(false)).await,
            Command::Leave if in_channel => {
                in_channel = false;
                send(Event::Left).await;
            }
            Command::SetAudioApp { app } => send(Event::AudioApps { apps: audio_apps(), selected: app, ask: false }).await,
            Command::StartLive { .. } | Command::StopLive | Command::SetQuality { .. } | Command::Leave => {}
        }
    }
}

/// Apps playing sound, for the "De qual app vem o som?" panel.
pub fn audio_apps() -> Vec<AudioApp> {
    [("firefox", "Firefox"), ("spotify", "Spotify"), ("mpv", "mpv")].map(|(id, label)| AudioApp { id: id.into(), label: label.into() }).into()
}

/// Monitors and windows, for the panel that picks what to share. Enough windows
/// for the list to scroll, and a title too long to fit.
pub fn sources() -> SourceList {
    let monitor = |n: u32, label: &str| MonitorEntry { label: label.into(), target: CaptureTarget::Monitor(DesktopOutput { adapter: 0, output: n }) };
    let titles = [
        "Mozilla Firefox",
        "relatorio-final-versao-revisada-pelo-pessoal-do-financeiro.xlsx - Excel",
        "Visual Studio Code",
        "Spotify Premium",
        "Bloco de notas",
        "Explorador de Arquivos",
        "Calculadora",
        "Prompt de Comando",
        "Paint",
    ];
    SourceList {
        monitors: vec![monitor(0, "Tela 1 (principal), 2560×1440"), monitor(1, "Tela 2, 1920×1080")],
        windows: titles
            .iter()
            .enumerate()
            .map(|(i, t)| WindowEntry { title: (*t).to_owned(), target: CaptureTarget::Window { hwnd: i as u64 + 1, title: (*t).to_owned() } })
            .collect(),
        no_windows: None,
    }
}

fn demo_viewers() -> Vec<Viewer> {
    ["Ana Souza", "Bruno", "Carla", "Duda Lima", "Edu", "Fê", "Gui"]
        .iter()
        .enumerate()
        .map(|(i, n)| Viewer { name: n.to_string(), supported: i != 3 })
        .collect()
}
