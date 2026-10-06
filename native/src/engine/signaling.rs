//! Cliente do servidor de sinalização do PeerJS (o mesmo que o site usa).
//!
//! Protocolo: WebSocket em `wss://SERVIDOR/peer/peerjs?key=peerjs&id=..&token=..`,
//! mensagens JSON `{type, src|dst, payload}`. Batimento a cada 5 s.

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as Ws;

#[derive(Debug, Clone)]
pub enum Signal {
    Open,
    IdTaken,
    Error(String),
    Offer { src: String, payload: Value },
    Answer { src: String, payload: Value },
    Candidate { src: String, payload: Value },
    /// O destino não existe ou saiu (o PeerJS chama de EXPIRE/LEAVE).
    Gone { peer: String },
    Closed,
}

#[derive(Clone)]
pub struct Signaling {
    out: mpsc::UnboundedSender<String>,
}

impl Signaling {
    /// Conecta e repassa cada mensagem para `events`.
    pub async fn connect(server: &str, id: &str, events: mpsc::Sender<Signal>) -> Result<Self, String> {
        let token: String = (0..10).map(|_| char::from(b'a' + rand::random_range(0..26u8))).collect();
        let url = format!("wss://{server}/peer/peerjs?key=peerjs&id={id}&token={token}&version=1.5.5");
        let (ws, _) = tokio_tungstenite::connect_async(&url).await.map_err(|e| format!("sinalização: {e}"))?;
        let (mut write, mut read) = ws.split();
        let (out, mut rx) = mpsc::unbounded_channel::<String>();

        tokio::spawn(async move {
            let mut beat = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                tokio::select! {
                    msg = rx.recv() => match msg {
                        Some(text) => if write.send(Ws::Text(text.into())).await.is_err() { break },
                        None => { let _ = write.close().await; break }
                    },
                    _ = beat.tick() => {
                        if write.send(Ws::Text(r#"{"type":"HEARTBEAT"}"#.into())).await.is_err() { break }
                    }
                }
            }
        });

        tokio::spawn(async move {
            while let Some(Ok(msg)) = read.next().await {
                let Ws::Text(text) = msg else { continue };
                let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                let src = v["src"].as_str().unwrap_or_default().to_owned();
                let payload = v["payload"].clone();
                let signal = match v["type"].as_str().unwrap_or_default() {
                    "OPEN" => Signal::Open,
                    "ID-TAKEN" => Signal::IdTaken,
                    "ERROR" | "INVALID-KEY" => Signal::Error(payload["msg"].as_str().unwrap_or("erro no servidor").to_owned()),
                    "OFFER" => Signal::Offer { src, payload },
                    "ANSWER" => Signal::Answer { src, payload },
                    "CANDIDATE" => Signal::Candidate { src, payload },
                    "EXPIRE" | "LEAVE" => Signal::Gone { peer: src },
                    _ => continue,
                };
                if events.send(signal).await.is_err() {
                    return;
                }
            }
            let _ = events.send(Signal::Closed).await;
        });

        Ok(Self { out })
    }

    pub fn send(&self, kind: &str, dst: &str, payload: Value) {
        let _ = self.out.send(json!({ "type": kind, "dst": dst, "payload": payload }).to_string());
    }
}

/// Servidores ICE (STUN e TURN com credencial temporária) do próprio servidor.
pub async fn ice_servers(server: &str) -> Result<Vec<webrtc::peer_connection::RTCIceServer>, String> {
    let url = format!("https://{server}/api/ice");
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(8)).build().map_err(|e| e.to_string())?;
    let mut last = String::new();
    for _ in 0..2 {
        match client.get(&url).send().await.and_then(|r| r.error_for_status()) {
            Ok(r) => {
                let v: Value = r.json().await.map_err(|e| e.to_string())?;
                let list = v["iceServers"].as_array().cloned().unwrap_or_default();
                return Ok(list
                    .into_iter()
                    .map(|s| webrtc::peer_connection::RTCIceServer {
                        urls: match &s["urls"] {
                            Value::String(u) => vec![u.clone()],
                            Value::Array(a) => a.iter().filter_map(|u| u.as_str().map(str::to_owned)).collect(),
                            _ => vec![],
                        },
                        username: s["username"].as_str().unwrap_or_default().to_owned(),
                        credential: s["credential"].as_str().unwrap_or_default().to_owned(),
                        ..Default::default()
                    })
                    .collect());
            }
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!("não consegui falar com o servidor do Telinha ({last})"))
}
