//! Rich Presence: shows on the Discord profile that you are in a Telinha
//! channel (or streaming), with a button that leads to the channel. Talks to
//! the Discord running on the computer over local IPC, the same as discord-rpc:
//! no server and no bot. Works with official Discord and with Vesktop (arRPC).
//!
//! The image is the application icon in the Developer Portal.
//! The application ID comes from TELINHA_DISCORD_ID (at build time, like the
//! server, or from the environment). Without it, none of this turns on.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;

#[derive(Debug, Clone, PartialEq)]
pub struct Presence {
    pub invite: String,
    pub live: bool,
    pub viewers: usize,
    /// Since when (Unix seconds): Discord shows the elapsed time.
    pub since: i64,
}

pub fn client_id() -> Option<String> {
    std::env::var("TELINHA_DISCORD_ID")
        .ok()
        .or_else(|| option_env!("TELINHA_DISCORD_ID").map(str::to_owned))
        .filter(|s| !s.trim().is_empty())
}

/// Keeps the profile up to date with `rx` while the app runs. If Discord is
/// not open, retries from time to time.
pub async fn run(mut rx: watch::Receiver<Option<Presence>>) {
    let Some(id) = client_id() else { return };
    loop {
        if let Some(io) = connect().await {
            match session(io, &id, &mut rx).await {
                Ok(()) => return,
                Err(e) => tracing::debug!("Discord: {e}"),
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(15)) => {}
            changed = rx.changed() => if changed.is_err() { return },
        }
    }
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;

async fn session(io: Box<dyn Io>, id: &str, rx: &mut watch::Receiver<Option<Presence>>) -> Result<(), String> {
    let (mut read, mut write) = tokio::io::split(io);
    send(&mut write, OP_HANDSHAKE, &json!({ "v": 1, "client_id": id })).await?;
    match recv(&mut read).await? {
        (OP_FRAME, v) if v["evt"] == "READY" => {
            tracing::info!(usuario = %v["data"]["user"]["username"].as_str().unwrap_or("?"), "Discord connected");
        }
        (_, v) => return Err(format!("Discord refused: {}", v["message"].as_str().unwrap_or("no reason"))),
    }

    // Replies and errors arrive here; they only matter for the log and to detect a drop.
    let (closed_tx, mut closed) = tokio::sync::oneshot::channel::<String>();
    tokio::spawn(async move {
        let why = loop {
            match recv(&mut read).await {
                Ok((OP_CLOSE, v)) => break format!("Discord closed: {}", v["message"].as_str().unwrap_or("")),
                Ok((_, v)) if v["evt"] == "ERROR" => tracing::warn!("Discord: {}", v["data"]["message"].as_str().unwrap_or("error")),
                Ok((_, v)) => tracing::debug!(resposta = %v, "Discord"),
                Err(e) => break e,
            }
        };
        let _ = closed_tx.send(why);
    });

    let mut nonce = 0u64;
    loop {
        let presence = rx.borrow_and_update().clone();
        nonce += 1;
        let cmd = json!({
            "cmd": "SET_ACTIVITY",
            "args": { "pid": std::process::id(), "activity": presence.as_ref().map(activity) },
            "nonce": nonce.to_string(),
        });
        send(&mut write, OP_FRAME, &cmd).await?;
        tokio::select! {
            changed = rx.changed() => if changed.is_err() {
                // App closing: clear the profile before leaving.
                let clear = json!({ "cmd": "SET_ACTIVITY", "args": { "pid": std::process::id() }, "nonce": "fim" });
                let _ = send(&mut write, OP_FRAME, &clear).await;
                return Ok(());
            },
            why = &mut closed => return Err(why.unwrap_or_default()),
        }
    }
}

fn activity(p: &Presence) -> Value {
    let state = match p.viewers {
        0 => "Ninguém assistindo ainda".to_owned(),
        1 => "1 pessoa assistindo".to_owned(),
        n => format!("{n} pessoas assistindo"),
    };
    json!({
        "details": if p.live { "Transmitindo a tela" } else { "Num canal, sem transmitir" },
        "state": state,
        "timestamps": { "start": p.since },
        "buttons": [{ "label": if p.live { "Assistir" } else { "Entrar no canal" }, "url": p.invite }],
    })
}

async fn send(w: &mut (impl AsyncWrite + Unpin), op: u32, v: &Value) -> Result<(), String> {
    let body = serde_json::to_vec(v).map_err(|e| e.to_string())?;
    let mut frame = Vec::with_capacity(8 + body.len());
    frame.extend_from_slice(&op.to_le_bytes());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    w.write_all(&frame).await.map_err(|e| e.to_string())
}

async fn recv(r: &mut (impl AsyncRead + Unpin)) -> Result<(u32, Value), String> {
    let mut head = [0u8; 8];
    r.read_exact(&mut head).await.map_err(|e| e.to_string())?;
    let op = u32::from_le_bytes(head[..4].try_into().unwrap());
    let len = u32::from_le_bytes(head[4..].try_into().unwrap()) as usize;
    if len > 1 << 20 {
        return Err("reply too large".into());
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await.map_err(|e| e.to_string())?;
    Ok((op, serde_json::from_slice(&body).unwrap_or(Value::Null)))
}

/// The discord-ipc-N socket, wherever Discord (or Vesktop, in Flatpak
/// or Snap) created it.
#[cfg(unix)]
async fn connect() -> Option<Box<dyn Io>> {
    let mut bases: Vec<std::path::PathBuf> = ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"].iter().filter_map(std::env::var_os).map(Into::into).collect();
    bases.push("/tmp".into());
    let subdirs = ["", "app/com.discordapp.Discord", "app/dev.vencord.Vesktop", ".flatpak/dev.vencord.Vesktop/xdg-run", "snap.discord"];
    for base in &bases {
        for sub in subdirs {
            for n in 0..10 {
                let path = base.join(sub).join(format!("discord-ipc-{n}"));
                if let Ok(s) = tokio::net::UnixStream::connect(&path).await {
                    return Some(Box::new(s));
                }
            }
        }
    }
    None
}

#[cfg(windows)]
async fn connect() -> Option<Box<dyn Io>> {
    for n in 0..10 {
        if let Ok(p) = tokio::net::windows::named_pipe::ClientOptions::new().open(format!(r"\\.\pipe\discord-ipc-{n}")) {
            return Some(Box::new(p));
        }
    }
    None
}
