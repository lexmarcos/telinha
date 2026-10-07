//! Preferences saved across app launches (quality, name, server, bubble
//! position). Stored as JSON in the system config directory.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Resolution {
    #[serde(rename = "720")]
    P720,
    #[serde(rename = "1080")]
    P1080,
    #[serde(rename = "1440")]
    P1440,
    Original,
}

impl Resolution {
    pub fn height(self) -> Option<u32> {
        match self {
            Self::P720 => Some(720),
            Self::P1080 => Some(1080),
            Self::P1440 => Some(1440),
            Self::Original => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    /// Keeps the frame rate; if the network gets tight, lowers the bitrate.
    Fluidez,
    /// Keeps the image sharp; if the network gets tight, sends fewer frames.
    Nitidez,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Quality {
    pub resolution: Resolution,
    pub fps: u32,
    pub priority: Priority,
    pub system_audio: bool,
}

impl Default for Quality {
    fn default() -> Self {
        Self { resolution: Resolution::Original, fps: 60, priority: Priority::Fluidez, system_audio: true }
    }
}

impl Quality {
    /// Output size, never upscaling past the screen and keeping the aspect ratio.
    pub fn output_size(&self, native: (u32, u32)) -> (u32, u32) {
        let (w, h) = native;
        let target = self.resolution.height().map_or(h, |t| t.min(h));
        let tw = ((target as f64 * w as f64 / h as f64) / 2.0).round() as u32 * 2;
        (tw, target & !1)
    }

    /// Same formula as the site: ~0.08 bits per pixel, between 2.5 and 20 Mb/s.
    pub fn bitrate(&self, native: (u32, u32)) -> u32 {
        // Network testing only: forces a fixed bitrate.
        if let Some(b) = std::env::var("TELINHA_TAXA").ok().and_then(|v| v.parse().ok()) {
            return b;
        }
        let (w, h) = self.output_size(native);
        let bps = w as f64 * h as f64 * self.fps as f64 * 0.08;
        bps.clamp(2.5e6, 20e6) as u32
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub quality: Quality,
    #[serde(default)]
    pub name: String,
    /// Last server used (comes from the invite link).
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub position: Option<(f32, f32)>,
    /// Show on the Discord profile that the user is in a channel (Rich Presence).
    #[serde(default = "yes")]
    pub discord: bool,
    /// Discord login (done through the Telinha bot): session and name.
    #[serde(default)]
    pub discord_session: Option<String>,
    #[serde(default)]
    pub discord_name: Option<String>,
    /// With login: only people in the same voice call can watch.
    #[serde(default = "yes")]
    pub call_only: bool,
    /// Capture portal token (Linux) so it does not ask again.
    #[serde(default)]
    pub capture_token: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self { quality: Quality::default(), name: String::new(), server: None, position: None, discord: true, discord_session: None, discord_name: None, call_only: true, capture_token: None }
    }
}

fn path() -> Option<std::path::PathBuf> {
    directories::ProjectDirs::from("online", "Telinha", "telinha").map(|d| d.config_dir().join("config.json"))
}

impl Config {
    pub fn load() -> Self {
        let mut cfg: Config = path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if cfg.name.trim().is_empty() {
            cfg.name = whoami();
        }
        if cfg.server.is_none() {
            cfg.server = option_env!("TELINHA_SERVER").map(str::to_owned);
        }
        cfg
    }

    pub fn save(&self) {
        // Test runs (mock engine, screenshot script) do not touch the real
        // preferences: the fake server would end up in the config.
        if std::env::var_os("TELINHA_MOCK").is_some() {
            return;
        }
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(p, json);
        }
    }
}

fn whoami() -> String {
    let raw = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default();
    let mut c = raw.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_else(|| "Alguém".into())
}
