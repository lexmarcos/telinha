//! What can be captured, and the decisions about it that need no operating
//! system: which windows are worth listing, how a window's process is found,
//! which filter captures what. Windows lists sources in the app's own panel;
//! on Linux the system portal picks, so the target there is always `Default`.

// Most of this only runs on Windows, and is kept here to be testable anywhere.
#![allow(dead_code)]

use std::time::Instant;

/// The DXGI output `ddagrab` opens: its `output_idx` counts the outputs of one
/// adapter, in `EnumOutputs` order (vsrc_ddagrab.c), so the two go together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DesktopOutput {
    pub adapter: u32,
    pub output: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CaptureTarget {
    /// No picker of the app's own: the first monitor on Windows, the system
    /// portal on Linux. What headless runs and the test source use.
    #[default]
    Default,
    Monitor(DesktopOutput),
    Window {
        /// HWND, as a number so the type exists on every system.
        hwnd: u64,
        title: String,
    },
}

/// What a capture turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureKind {
    /// A whole monitor.
    Monitor,
    /// A single window, and where its sound can come from.
    Window { title: Option<String>, sound: WindowSound },
}

impl CaptureKind {
    pub fn is_window(&self) -> bool {
        matches!(self, Self::Window { .. })
    }
}

/// Where a captured window's sound can come from. There is deliberately no
/// "the whole computer" here: see `audio::plan`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowSound {
    /// This process and its children (Windows).
    Process(u32),
    /// Nothing says which process it is (Linux: the portal does not tell), so
    /// the person picks the app.
    AskApp,
    /// It is Discord: the people in the call would hear themselves back.
    Discord,
    /// The process could not be found.
    Unknown,
}

/// The system gave no way to open the capture, or the person gave up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// The person cancelled in the picker: nothing to report.
    Cancelled,
    Failed(String),
}

/// A monitor in the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorEntry {
    pub label: String,
    pub target: CaptureTarget,
}

/// A window in the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowEntry {
    pub title: String,
    pub target: CaptureTarget,
}

/// Everything the picker shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SourceList {
    pub monitors: Vec<MonitorEntry>,
    pub windows: Vec<WindowEntry>,
    /// Why windows are not offered, when they are not.
    pub no_windows: Option<String>,
}

/* ---------------- Windows: which windows to list ---------------- */

/// What the system says about a top-level window.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowFacts {
    pub title: String,
    pub class: String,
    pub visible: bool,
    /// Hidden by the compositor (`DWMWA_CLOAKED`): suspended apps, other desktops.
    pub cloaked: bool,
    pub tool_window: bool,
    pub ours: bool,
    pub width: i32,
    pub height: i32,
}

/// The desktop and the taskbar are windows too, and have titles.
const SHELL_CLASSES: [&str; 3] = ["Progman", "WorkerW", "Shell_TrayWnd"];

/// Whether a window goes in the picker. A minimized one does: it reports no
/// size worth checking, and picking it brings it back.
pub fn listable(w: &WindowFacts, minimized: bool) -> bool {
    w.visible
        && !w.cloaked
        && !w.tool_window
        && !w.ours
        && !w.title.trim().is_empty()
        && !SHELL_CLASSES.contains(&w.class.as_str())
        && (minimized || (w.width > 0 && w.height > 0))
}

/// Windows.Graphics.Capture by window handle exists since Windows 10 1903, but
/// isolating a process's sound only since 2004 (build 19041). Before that a
/// window could be shown but not heard on its own, so it is not offered at all.
pub const WINDOW_MIN_BUILD: u32 = 19041;

pub fn windows_supported(build: u32) -> bool {
    build >= WINDOW_MIN_BUILD
}

/// UWP windows belong to ApplicationFrameHost, which plays nothing: the app is
/// the process behind one of its child windows. `None` when no such child
/// exists, which happens while the app is suspended.
pub fn real_pid(pid: u32, exe: &str, child_pids: &[u32]) -> Option<u32> {
    if !exe.eq_ignore_ascii_case("applicationframehost.exe") {
        return Some(pid);
    }
    child_pids.iter().copied().find(|c| *c != pid && *c != 0)
}

/// Discord's executables, on both systems' spelling of the name.
pub fn is_discord(name: &str) -> bool {
    let name = name.to_lowercase();
    ["discord", "vesktop", "vencord", "webcord", "armcord", "legcord", "equibop"].iter().any(|d| name.contains(d))
}

pub fn window_sound(pid: Option<u32>, exe: &str, child_pids: &[u32]) -> WindowSound {
    if is_discord(exe) {
        return WindowSound::Discord;
    }
    match pid.and_then(|p| real_pid(p, exe, child_pids)) {
        Some(p) => WindowSound::Process(p),
        None => WindowSound::Unknown,
    }
}

/* ---------------- Filters ---------------- */

/// A monitor: Desktop Duplication, already on the GPU.
pub fn ddagrab_filter(output: DesktopOutput, fps: u32) -> String {
    format!("ddagrab=output_idx={}:framerate={fps}:draw_mouse=1", output.output)
}

/// A window, through Windows.Graphics.Capture (FFmpeg 9's `gfxcapture`, D3D11
/// BGRA frames like `ddagrab`). The frame size is fixed when the filter opens:
/// even by rounding down, and while the window is a different size the picture
/// is fitted inside instead of cropped, until the capture reopens at the new
/// size. It only sends a frame when the window changes, and at most `MAX_FPS`;
/// the video thread holds the stream to the chosen rate.
pub fn gfxcapture_filter(hwnd: u64) -> String {
    format!("gfxcapture=hwnd={hwnd}:max_framerate={MAX_FPS}:capture_cursor=1:width=-2:height=-2:resize_mode=scale_aspect")
}

/// Fixed so a quality change does not reopen the capture.
const MAX_FPS: u32 = 60;

/* ---------------- Resizing ---------------- */

/// A window being dragged changes size on every frame, and reopening the
/// encoder for each costs a keyframe and tens of milliseconds. A new size is
/// acted on once it has held for `SETTLE`, or after `LONGEST` of continuous
/// dragging so the viewers do not stare at the old picture for the whole drag.
#[derive(Debug, Default)]
pub struct Resize {
    pending: Option<((u32, u32), Instant, Instant)>,
}

const SETTLE: std::time::Duration = std::time::Duration::from_millis(250);
const LONGEST: std::time::Duration = std::time::Duration::from_millis(1500);

impl Resize {
    /// A frame (or a measure) of `size` came, other than the current one.
    pub fn saw(&mut self, size: (u32, u32), now: Instant) {
        self.pending = match self.pending {
            Some((old, at, first)) if old == size => Some((old, at, first)),
            Some((_, _, first)) => Some((size, now, first)),
            None => Some((size, now, now)),
        };
    }

    /// Whether the new size can be acted on now.
    pub fn due(&self, now: Instant) -> bool {
        self.pending.is_some_and(|(_, at, first)| now.saturating_duration_since(at) >= SETTLE || now.saturating_duration_since(first) >= LONGEST)
    }

    /// The size came back, or was acted on.
    pub fn clear(&mut self) {
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn janela() -> WindowFacts {
        WindowFacts { title: "Bloco de notas".into(), class: "Notepad".into(), visible: true, width: 800, height: 600, ..Default::default() }
    }

    #[test]
    fn janela_comum_entra_na_lista() {
        assert!(listable(&janela(), false));
    }

    #[test]
    fn filtro_de_janelas_deixa_de_fora() {
        let casos: [(&str, fn(&mut WindowFacts)); 8] = [
            ("invisível", |w| w.visible = false),
            ("encoberta", |w| w.cloaked = true),
            ("ferramenta", |w| w.tool_window = true),
            ("o próprio Telinha", |w| w.ours = true),
            ("sem título", |w| w.title.clear()),
            ("título em branco", |w| w.title = "   ".into()),
            ("área de trabalho", |w| w.class = "Progman".into()),
            ("sem tamanho", |w| w.width = 0),
        ];
        for (nome, muda) in casos {
            let mut w = janela();
            muda(&mut w);
            assert!(!listable(&w, false), "{nome}");
        }
    }

    #[test]
    fn minimizada_entra_mesmo_sem_tamanho() {
        let mut w = janela();
        w.width = 0;
        w.height = 0;
        assert!(listable(&w, true));
    }

    #[test]
    fn janela_so_a_partir_do_windows_10_2004() {
        assert!(!windows_supported(18363));
        assert!(!windows_supported(19040));
        assert!(windows_supported(19041));
        assert!(windows_supported(22631));
    }

    #[test]
    fn app_uwp_usa_o_processo_filho() {
        assert_eq!(real_pid(10, "firefox.exe", &[99]), Some(10));
        assert_eq!(real_pid(10, "ApplicationFrameHost.exe", &[10, 0, 42]), Some(42));
        assert_eq!(real_pid(10, "applicationframehost.exe", &[10]), None);
    }

    #[test]
    fn som_da_janela() {
        assert_eq!(window_sound(Some(7), "spotify.exe", &[]), WindowSound::Process(7));
        assert_eq!(window_sound(Some(7), "Discord.exe", &[]), WindowSound::Discord);
        assert_eq!(window_sound(None, "spotify.exe", &[]), WindowSound::Unknown);
        assert_eq!(window_sound(Some(7), "ApplicationFrameHost.exe", &[7]), WindowSound::Unknown);
    }

    #[test]
    fn filtros_do_ffmpeg() {
        assert_eq!(ddagrab_filter(DesktopOutput { adapter: 1, output: 2 }, 60), "ddagrab=output_idx=2:framerate=60:draw_mouse=1");
        let g = gfxcapture_filter(66052);
        assert!(g.starts_with("gfxcapture=hwnd=66052:"), "{g}");
        assert!(g.contains("width=-2:height=-2"), "even sizes: {g}");
        assert!(g.contains("resize_mode=scale_aspect"), "{g}");
    }

    #[test]
    fn arrasto_nao_reabre_a_cada_pixel() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut r = Resize::default();
        assert!(!r.due(ms(0)), "nothing pending");
        // A size every 16 ms for a while: never settled.
        let mut opened_at = None;
        for k in 0..60u64 {
            r.saw((800 + k as u32, 600), ms(k * 16));
            if r.due(ms(k * 16)) {
                opened_at = Some(k * 16);
                break;
            }
        }
        assert_eq!(opened_at, None, "under the longest wait");
        // The drag goes on past the longest wait: one reopen, then a fresh count.
        r.saw((900, 600), ms(1500));
        assert!(r.due(ms(1500)));
        r.clear();
        r.saw((901, 600), ms(1516));
        assert!(!r.due(ms(1516)));
        // It stops: due once it has held still.
        assert!(!r.due(ms(1700)));
        assert!(r.due(ms(1766)));
    }

    #[test]
    fn mesmo_tamanho_repetido_conta_desde_a_primeira_vez() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut r = Resize::default();
        r.saw((800, 600), ms(0));
        r.saw((800, 600), ms(100));
        r.saw((800, 600), ms(200));
        assert!(r.due(ms(250)));
    }

    #[test]
    fn voltar_ao_tamanho_cancela() {
        let t0 = Instant::now();
        let mut r = Resize::default();
        r.saw((800, 600), t0);
        r.clear();
        assert!(!r.due(t0 + Duration::from_secs(10)));
    }
}
