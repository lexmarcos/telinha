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

/// The smallest output: NVENC refuses anything under 145x49 for H.264 and 129x33
/// for HEVC, and other encoders have limits of their own around 16 to 32.
const MIN_OUTPUT: (u32, u32) = (160, 90);
/// The largest side: H.264 hardware encoders stop at 4096 (a few at more). A
/// window spread over two monitors can pass it.
const MAX_OUTPUT: u32 = 4096;

impl Quality {
    /// Output size, never upscaling past the screen and keeping the aspect ratio.
    /// A window can be any size at all, so this is also what keeps it within what
    /// the encoders take: even on both sides and between `MIN_OUTPUT` and
    /// `MAX_OUTPUT`. Only a window too narrow for that loses its proportions.
    pub fn output_size(&self, native: (u32, u32)) -> (u32, u32) {
        let (w, h) = (native.0.max(1), native.1.max(1));
        let th = (self.resolution.height().map_or(h, |t| t.min(h)) & !1).max(2) as f64;
        let tw = th * w as f64 / h as f64;
        let grow = (MIN_OUTPUT.0 as f64 / tw).max(MIN_OUTPUT.1 as f64 / th).max(1.0);
        let even = |v: f64| (((v * grow / 2.0).round() as u32) * 2).min(MAX_OUTPUT);
        (even(tw), even(th))
    }

    /// Same formula as the site: ~0.08 bits per pixel (0.1 in Nitidez, where
    /// text needs it most), from 2.5 up to 45 Mb/s (50 in Nitidez). The old
    /// 20 Mb/s ceiling left 4K60 at ~0.04 bits per pixel; now it gets ~40.
    pub fn bitrate(&self, native: (u32, u32)) -> u32 {
        // Network testing only: forces a fixed bitrate.
        if let Some(b) = std::env::var("TELINHA_TAXA").ok().and_then(|v| v.parse().ok()) {
            return b;
        }
        let (w, h) = self.output_size(native);
        let (bpp, cap) = match self.priority {
            Priority::Fluidez => (0.08, 45e6),
            Priority::Nitidez => (0.1, 50e6),
        };
        let bps = w as f64 * h as f64 * self.fps as f64 * bpp;
        bps.clamp(2.5e6, cap) as u32
    }
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
}

impl Default for Config {
    fn default() -> Self {
        Self { quality: Quality::default(), name: String::new(), server: None, position: None }
    }
}

/// Server baked in at build time (TELINHA_SERVER). Public builds leave it empty:
/// the app then asks for an invite link on first run.
pub fn default_server() -> Option<&'static str> {
    option_env!("TELINHA_SERVER").filter(|s| !s.is_empty())
}

/// The app's folders: Linux ~/.config/telinha and ~/.local/share/telinha,
/// Windows %APPDATA%\Telinha\telinha (no reverse-domain qualifier: it only
/// matters on macOS, and the app is not tied to any server's domain).
pub fn dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "Telinha", "telinha")
}

fn path() -> Option<std::path::PathBuf> {
    dirs().map(|d| d.config_dir().join("config.json"))
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
            cfg.server = default_server().map(str::to_owned);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn q(resolution: Resolution) -> Quality {
        Quality { resolution, fps: 60, priority: Priority::Fluidez, system_audio: false }
    }

    #[test]
    fn tamanhos_de_tela_nao_mudam() {
        let o = q(Resolution::Original);
        assert_eq!(o.output_size((1920, 1080)), (1920, 1080));
        assert_eq!(o.output_size((3840, 2160)), (3840, 2160));
        assert_eq!(o.output_size((2560, 1440)), (2560, 1440));
        assert_eq!(q(Resolution::P720).output_size((1920, 1080)), (1280, 720));
        assert_eq!(q(Resolution::P1080).output_size((3840, 2160)), (1920, 1080));
        assert_eq!(q(Resolution::P1440).output_size((1920, 1080)), (1920, 1080));
    }

    #[test]
    fn janela_sai_sempre_par_e_dentro_dos_limites() {
        let o = q(Resolution::Original);
        // Odd sizes round to even.
        assert_eq!(o.output_size((801, 601)), (800, 600));
        assert_eq!(o.output_size((1367, 769)), (1366, 768));
        // Small windows grow to the minimum, keeping the proportions.
        assert_eq!(o.output_size((100, 60)), (160, 96));
        assert_eq!(o.output_size((1, 1)), (160, 160));
        // Nothing divides by zero while a window has no size yet.
        assert_eq!(o.output_size((0, 0)), (160, 160));
        // Absurd proportions stop at the largest side.
        assert_eq!(o.output_size((6000, 400)).0, 4096);
        assert_eq!(o.output_size((3000, 10)), (4096, 90));
        for (w, h) in [(3, 7), (640, 481), (1921, 1081), (5000, 300), (161, 91), (160, 90), (333, 1000), (17, 4000)] {
            for r in [Resolution::Original, Resolution::P720, Resolution::P1080, Resolution::P1440] {
                let (ow, oh) = q(r).output_size((w, h));
                assert!(ow % 2 == 0 && oh % 2 == 0, "{w}x{h} {r:?}: {ow}x{oh}");
                assert!((160..=4096).contains(&ow) && (90..=4096).contains(&oh), "{w}x{h} {r:?}: {ow}x{oh}");
            }
        }
    }

    #[test]
    fn taxa_cresce_com_a_resolucao() {
        let q = |fps, priority| Quality { resolution: Resolution::Original, fps, priority, system_audio: false };
        let mb = |q: Quality, size| (q.bitrate(size) as f64 / 1e5).round() / 10.0;
        assert_eq!(mb(q(60, Priority::Fluidez), (1920, 1080)), 10.0);
        assert_eq!(mb(q(60, Priority::Fluidez), (3840, 2160)), 39.8);
        assert_eq!(mb(q(60, Priority::Nitidez), (3840, 2160)), 49.8);
        assert_eq!(mb(q(60, Priority::Fluidez), (5120, 2880)), 45.0);
        assert_eq!(mb(q(30, Priority::Fluidez), (1280, 720)), 2.5);
    }

    /// A config.json saved by a version that still had the login: its fields
    /// are ignored, the rest stays, and the next save leaves them out.
    #[test]
    fn config_antiga_com_login_carrega_sem_perder_o_resto() {
        let old = r#"{
            "quality": { "resolution": "1080", "fps": 30, "priority": "nitidez", "system_audio": false },
            "name": "Ana",
            "server": "exemplo.test",
            "position": [10.0, 20.0],
            "discord_session": "abc",
            "discord_name": "Ana#1",
            "call_only": false,
            "capture_token": "token"
        }"#;
        let c: Config = serde_json::from_str(old).expect("old config must keep loading");
        assert_eq!(c.quality, Quality { resolution: Resolution::P1080, fps: 30, priority: Priority::Nitidez, system_audio: false });
        assert_eq!(c.name, "Ana");
        assert_eq!(c.server.as_deref(), Some("exemplo.test"));
        assert_eq!(c.position, Some((10.0, 20.0)));
        let saved = serde_json::to_value(&c).unwrap();
        assert!(saved.get("discord_session").is_none() && saved.get("discord_name").is_none() && saved.get("call_only").is_none());
    }
}
