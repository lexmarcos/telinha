//! Discord gatekeeper: when on, only people in the same voice call as the
//! streamer can watch.
//!
//! When the stream starts, the app notifies the Telinha bot (bot/, behind the
//! server at /discord). The bot finds the call the person is in and posts an
//! "Assistir" (Watch) button in its chat. Whoever clicks it while in the call
//! gets a signed pass (Ed25519) and hands it to the app over a direct
//! connection. The app checks the signature, room, recipient and expiry, and
//! only then sends video.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde_json::{Value, json};

/// Bot reply to "started": the pass key, the stream id and whether the
/// Watch button was posted in the call chat.
type Opened = (Vec<u8>, Option<String>, bool);

pub struct Gate {
    /// Public key for passes (comes from the bot). Without it, nobody gets in.
    key: Option<Vec<u8>>,
    session: String,
    live: Option<String>,
    /// Who has shown a pass: PeerJS id → (Discord id, name).
    pub allowed: HashMap<String, (String, String)>,
    /// For retries: server and the body of the "started" request.
    server: String,
    body: Value,
    attempt: Option<tokio::task::JoinHandle<Result<Opened, String>>>,
    next_try: Instant,
    failures: u32,
}

/// Until the bot confirms (e.g. the stream started before joining the
/// call), retry at this interval.
const RETRY: Duration = Duration::from_secs(15);

/// Gatekeeper update to show in the bubble.
pub enum Change {
    Ready { announced: bool, after_failure: bool },
    Failed(String),
}

impl Gate {
    /// Turns on the gatekeeper and notifies the bot in the background. Until
    /// the bot confirms (not in a call, bot down), the gate stays closed:
    /// nobody watches. `poll` follows up and retries.
    pub fn open(server: &str, session: String, code: &str, me: &str) -> Self {
        let body = json!({ "sessao": session, "codigo": code, "peer": me });
        let mut gate = Self {
            key: None,
            session,
            live: None,
            allowed: HashMap::new(),
            server: server.to_owned(),
            body,
            attempt: None,
            next_try: Instant::now(),
            failures: 0,
        };
        gate.try_open();
        gate
    }

    fn try_open(&mut self) {
        let (server, body) = (self.server.clone(), self.body.clone());
        self.attempt = Some(tokio::spawn(async move {
            let v = post(&server, "ao-vivo", &body).await?;
            let key = v["chave"].as_str().and_then(|k| B64.decode(k).ok()).filter(|k| k.len() == 32).ok_or("O bot não mandou a chave dos passes.")?;
            Ok((key, v["transmissao"].as_str().map(str::to_owned), v["anunciado"].as_bool().unwrap_or(false)))
        }));
        self.next_try = Instant::now() + RETRY;
    }

    /// Called every second: collects the bot's reply and retries if it has
    /// not worked yet. Only returns changes (the first error and the success).
    pub async fn poll(&mut self) -> Option<Change> {
        if self.key.is_some() {
            return None;
        }
        if self.attempt.as_ref().is_some_and(|a| a.is_finished()) {
            let res = self.attempt.take()?.await.unwrap_or_else(|e| Err(e.to_string()));
            return match res {
                Ok((key, live, announced)) => {
                    self.key = Some(key);
                    self.live = live;
                    Some(Change::Ready { announced, after_failure: self.failures > 0 })
                }
                Err(e) => {
                    self.failures += 1;
                    (self.failures == 1).then_some(Change::Failed(e))
                }
            };
        }
        if self.attempt.is_none() && Instant::now() >= self.next_try {
            self.try_open();
        }
        None
    }

    /// Tells the bot the stream ended (the call announcement becomes "streamed for X min").
    pub fn close(self) {
        if let Some(a) = &self.attempt {
            a.abort();
        }
        let (server, body) = (self.server.clone(), json!({ "sessao": self.session, "transmissao": self.live }));
        tokio::spawn(async move {
            let _ = post(&server, "fim", &body).await;
        });
    }

    /// The bot has confirmed the stream (passes can be checked).
    pub fn ready(&self) -> bool {
        self.key.is_some()
    }

    /// Checks a pass. Returns (Discord id, name) of who may watch.
    pub fn check(&self, pass: &str, code: &str, me: &str) -> Result<(String, String), &'static str> {
        let key = self.key.as_ref().ok_or("A transmissão ainda não está ligada ao Discord.")?;
        let (body, sig) = pass.split_once('.').ok_or("Passe inválido.")?;
        let sig = B64.decode(sig).map_err(|_| "Passe inválido.")?;
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key).verify(body.as_bytes(), &sig).map_err(|_| "Passe falso.")?;
        let p: Value = B64.decode(body).ok().and_then(|b| serde_json::from_slice(&b).ok()).ok_or("Passe inválido.")?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        if p["s"] != code || p["p"] != me {
            return Err("Esse passe é de outra transmissão. Clique em Assistir de novo no chat da call.");
        }
        if p["e"].as_u64().unwrap_or(0) < now {
            return Err("Esse passe venceu. Clique em Assistir de novo no chat da call.");
        }
        let user = p["u"].as_str().ok_or("Passe inválido.")?.to_owned();
        let name = p["n"].as_str().unwrap_or("Alguém").chars().take(24).collect();
        Ok((user, name))
    }

    /// Admits this peer. A pass is valid for one connection only: if the same
    /// person joins from somewhere else, the previous one is dropped.
    pub fn admit(&mut self, peer: String, user: String, name: String) {
        self.allowed.retain(|_, (u, _)| *u != user);
        self.allowed.insert(peer, (user, name));
    }
}

async fn post(server: &str, path: &str, body: &Value) -> Result<Value, String> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().map_err(|e| e.to_string())?;
    // TELINHA_BOT_URL is only for tests (a local fake bot).
    let base = std::env::var("TELINHA_BOT_URL").unwrap_or_else(|_| format!("https://{server}"));
    let r = client.post(format!("{base}/discord/{path}")).json(body).send().await.map_err(|_| "O bot do Discord não respondeu.".to_owned())?;
    let ok = r.status().is_success();
    let v: Value = r.json().await.unwrap_or(Value::Null);
    if ok { Ok(v) } else { Err(v["erro"].as_str().unwrap_or("O bot do Discord recusou.").to_owned()) }
}

/* ---------------- Discord login (once) ---------------- */

/// Opens the login in the browser and waits for the bot to confirm. Returns (session, name).
pub async fn login(server: String) -> Result<(String, String), String> {
    let state: String = {
        use rand::Rng;
        let mut rng = rand::rng();
        (0..32).map(|_| char::from(b"abcdefghijklmnopqrstuvwxyz0123456789"[rng.random_range(0..36)])).collect()
    };
    open_browser(&format!("https://{server}/discord/login?estado={state}"))?;
    let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().map_err(|e| e.to_string())?;
    let url = format!("https://{server}/discord/login/resultado?estado={state}");
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let Ok(r) = client.get(&url).send().await else { continue };
        if r.status().as_u16() == 404 {
            continue; // the browser has not reached the bot yet
        }
        let v: Value = r.json().await.unwrap_or(Value::Null);
        if v["pronto"] == true {
            let session = v["sessao"].as_str().ok_or("resposta sem sessão")?.to_owned();
            return Ok((session, v["usuario"]["name"].as_str().unwrap_or("você").to_owned()));
        }
    }
    Err("O login com o Discord não terminou. Tente de novo.".into())
}

pub fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
    #[cfg(not(target_os = "windows"))]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    r.map(|_| ()).map_err(|e| format!("não abriu o navegador: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Passes signed with WebCrypto (Ed25519), the same way the bot does.
    const KEY: &str = "TyA0ANtPgkGKeaarn_qz5bkL6O2c19rHKOHUZfe2GDc";
    const OK: &str = "eyJ2IjoxLCJzIjoiNjc3OSIsInAiOiJwZWVyLWFiYyIsInUiOiI0MiIsIm4iOiJBbmEiLCJlIjo0MTAyNDQ0ODAwfQ.hY3a1wRFWNd5x-v7grV0ys6o_RpHqntt4hhLO5ddvJvhGAXZlyxmpXLbVRb-iFCDZnpqjeJyGkFCepnRZ2UmCA";
    const OLD: &str = "eyJ2IjoxLCJzIjoiNjc3OSIsInAiOiJwZWVyLWFiYyIsInUiOiI0MiIsIm4iOiJBbmEiLCJlIjoxMDAwfQ.atRq5SoX2fhJMgPreI53mFuL0bR7pTWtQQkXU9gXgG2Y1hFviIEsyiTkRW5Hwe1p1uyb8pW1SywqixRnptYtDg";

    fn gate() -> Gate {
        Gate {
            key: Some(B64.decode(KEY).unwrap()),
            session: String::new(),
            live: None,
            allowed: HashMap::new(),
            server: String::new(),
            body: Value::Null,
            attempt: None,
            next_try: Instant::now(),
            failures: 0,
        }
    }

    #[test]
    fn accepts_valid_pass() {
        assert_eq!(gate().check(OK, "6779", "peer-abc"), Ok(("42".into(), "Ana".into())));
    }

    #[test]
    fn rejects_other_room_other_recipient_expired_and_tampered() {
        let g = gate();
        assert!(g.check(OK, "1234", "peer-abc").is_err());
        assert!(g.check(OK, "6779", "outro-peer").is_err());
        assert!(g.check(OLD, "6779", "peer-abc").unwrap_err().contains("venceu"));
        // Changing the content (e.g. the name) invalidates the signature.
        let (_, sig) = OK.split_once('.').unwrap();
        let forged = format!("{}.{sig}", B64.encode(br#"{"v":1,"s":"6779","p":"peer-abc","u":"666","n":"Intruso","e":4102444800}"#));
        assert_eq!(g.check(&forged, "6779", "peer-abc"), Err("Passe falso."));
        // Without a key (bot down), nobody gets in.
        let closed = Gate { key: None, ..gate() };
        assert!(closed.check(OK, "6779", "peer-abc").is_err());
    }

    #[test]
    fn one_pass_is_valid_for_one_connection() {
        let mut g = gate();
        g.admit("peer-1".into(), "42".into(), "Ana".into());
        g.admit("peer-2".into(), "42".into(), "Ana".into());
        assert!(!g.allowed.contains_key("peer-1") && g.allowed.contains_key("peer-2"));
    }
}
