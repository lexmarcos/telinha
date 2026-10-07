//! Mock engine for developing and testing the UI: simulates opening and
//! joining a channel, people arriving, and the stream. Enabled with TELINHA_MOCK=1.

use std::time::Duration;

use tokio::sync::mpsc;

use super::{Command, EncoderInfo, Event, Viewer};

pub async fn run(mut rx: mpsc::Receiver<Command>, out: mpsc::Sender<Event>) {
    let send = async |e: Event| {
        let _ = out.send(e).await;
    };
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Command::Create { .. } | Command::Join { .. } => {
                send(Event::Connecting).await;
                tokio::time::sleep(Duration::from_millis(900)).await;
                send(Event::Joined { code: "4821".into(), invite: "https://exemplo/#4821".into(), server: "exemplo".into() }).await;
                send(Event::Screen { width: 2560, height: 1440 }).await;
                send(Event::Viewers(demo_viewers())).await;
            }
            Command::StartLive { .. } => {
                send(Event::Encoder(EncoderInfo { name: "NVENC".into(), hardware: true, width: 2560, height: 1440 })).await;
                send(Event::Live(true)).await;
            }
            Command::StopLive => send(Event::Live(false)).await,
            Command::SetQuality { .. } => {}
            Command::Leave => send(Event::Left).await,
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
