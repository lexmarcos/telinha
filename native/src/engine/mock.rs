//! Mock engine for developing and testing the UI: simulates opening and
//! joining a channel, people arriving, and the stream. Enabled with TELINHA_MOCK=1.
//! TELINHA_MOCK_FALHA makes one part fail: `servidor` (no known server), `conexao`
//! (opening a channel), `captura` (starting the stream) or `copia` (the clipboard,
//! which the app checks).

use std::time::Duration;

use tokio::sync::mpsc;

use super::{Command, EncoderInfo, Event, Viewer};

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
            Command::StartLive { .. } if in_channel => {
                // The capture picker: commands wait behind it, as in the real engine.
                tokio::time::sleep(Duration::from_millis(1000)).await;
                if fail == "captura" {
                    send(Event::Failed("A captura foi cancelada no seletor de tela.".into())).await;
                    continue;
                }
                send(Event::Encoder(EncoderInfo { name: "NVENC".into(), hardware: true, width: 2560, height: 1440 })).await;
                send(Event::Live(true)).await;
            }
            Command::StopLive if in_channel => send(Event::Live(false)).await,
            Command::Leave if in_channel => {
                in_channel = false;
                send(Event::Left).await;
            }
            Command::StartLive { .. } | Command::StopLive | Command::SetQuality { .. } | Command::Leave => {}
        }
    }
}

fn demo_viewers() -> Vec<Viewer> {
    ["Ana Souza", "Bruno", "Carla", "Duda Lima", "Edu", "Fê", "Gui"]
        .iter()
        .enumerate()
        .map(|(i, n)| Viewer { name: n.to_string(), supported: i != 3 })
        .collect()
}
