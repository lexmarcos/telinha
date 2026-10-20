//! Video pipeline: capture → filters (scale/GPU upload) → encoder.
//! Runs on its own thread and publishes H.264 frames ready to send.

pub mod encoders;
pub mod ffi;
pub mod h264;
pub mod source;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use ffmpeg_sys_next as ff;
use tokio::sync::broadcast;

use crate::config::Quality;
use crate::engine::EncoderInfo;
use encoders::{Candidate, Ctx, SourceKind};
use ffi::{Encoder, EncoderParams, Frame, Graph, HwDevice, HwFrames, InputSpec};
use source::{CpuFrame, CpuSource};

#[derive(Debug)]
pub struct EncodedFrame {
    pub data: Bytes,
    pub key: bool,
    /// WebCodecs codec string ("avc1.64002a").
    pub codec: String,
    pub width: u32,
    pub height: u32,
    /// When the image was captured (to measure encoding time).
    pub captured: Instant,
}

/// NVENC refused because the NVIDIA driver is older than the bundled FFmpeg
/// requires (it asks for the API version it was built against). The engine
/// shows a notice in the bubble telling the user to update the driver.
pub static NVENC_DRIVER_TOO_OLD: AtomicBool = AtomicBool::new(false);
/// Minimum NVIDIA driver for the bundled FFmpeg's NVENC.
#[cfg(target_os = "windows")]
pub const NVENC_MIN_DRIVER: &str = "531";
#[cfg(not(target_os = "windows"))]
pub const NVENC_MIN_DRIVER: &str = "530";

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub captured: u64,
    pub skipped: u64,
    pub encoded: u64,
    pub encode_ms: Option<f32>,
    pub encoder: Option<EncoderInfo>,
    pub native: Option<(u32, u32)>,
    pub error: Option<String>,
}

/// Commands for the video thread, read on every frame.
#[derive(Default)]
pub struct Control {
    pub force_key: AtomicBool,
    pub stop: AtomicBool,
    pending: Mutex<Option<Settings>>,
    pub stats: Mutex<Stats>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub quality: Quality,
    /// Current bitrate (network adaptation may lower it below the ideal).
    pub bitrate: u32,
}

impl Control {
    pub fn request_key(&self) {
        self.force_key.store(true, Ordering::Relaxed);
    }

    pub fn reconfigure(&self, s: Settings) {
        *self.pending.lock().unwrap() = Some(s);
    }

    pub fn stats(&self) -> Stats {
        self.stats.lock().unwrap().clone()
    }
}

pub enum Source {
    Cpu(Box<dyn CpuSource>),
    #[cfg(target_os = "windows")]
    Desktop,
}

/// Starts streaming. Frames come out on `out`; `Control` changes quality,
/// requests keyframes and stops. The bitrate starts at the ideal for the
/// real screen size (`Quality::bitrate`), known once capture starts.
pub fn start(source: Source, quality: Quality, out: broadcast::Sender<Arc<EncodedFrame>>) -> Arc<Control> {
    ffi::silence_logs();
    let control = Arc::new(Control::default());
    let c = control.clone();
    std::thread::Builder::new()
        .name("telinha-video".into())
        .spawn(move || {
            let _streaming = crate::realtime::streaming();
            let _boost = crate::realtime::boost(crate::realtime::Task::Video);
            if let Err(e) = run(source, quality, &c, &out) {
                tracing::error!("video stopped: {e}");
                c.stats.lock().unwrap().error = Some(e.0);
            }
        })
        .expect("video thread");
    control
}

struct Pipeline {
    graph: Graph,
    encoder: Encoder,
    candidate: usize,
    out: (u32, u32),
    /// Incoming DMA-BUF frames (only with `SourceKind::Prime`).
    frames: Option<HwFrames>,
    _device: Option<HwDevice>,
}

fn open_encoder(cand: &Candidate, graph: &Graph, ctx: &Ctx, bitrate: u32) -> ffi::Result<Encoder> {
    let (w, h) = graph.out_size();
    let options = (cand.options)(ctx);
    Encoder::open(&EncoderParams {
        name: cand.encoder,
        width: w,
        height: h,
        fps: ctx.fps,
        bitrate,
        pix_fmt: graph.out_format(),
        hw_frames: graph.out_hw_frames(),
        options: &options,
    })
}

/// Tries candidates in order; the first that builds the graph and opens the
/// encoder wins. `start_at` skips the ones that already failed.
/// `pixel` is the pixel format (BGRX/RGBX), even when the pixels are on the GPU.
fn build(kind: SourceKind, native: (u32, u32), pixel: ff::AVPixelFormat, s: &Settings, start_at: usize) -> ffi::Result<Pipeline> {
    let out = s.quality.output_size(native);
    let ctx = Ctx { native, out, fps: s.quality.fps };
    let mut last = ffi::Error("nenhum codificador disponível".into());
    for (i, cand) in encoders::candidates().iter().enumerate().skip(start_at) {
        if cand.source != kind {
            continue;
        }
        let attempt = || -> ffi::Result<Pipeline> {
            let device = match (kind, cand.device) {
                #[cfg(target_os = "linux")]
                (SourceKind::Prime, _) => Some(HwDevice::create(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM, Some(&render_node()))?),
                (_, Some((t, d))) => Some(HwDevice::create(t, d)?),
                _ => None,
            };
            #[cfg(target_os = "linux")]
            let frames = match (kind, device.as_ref()) {
                (SourceKind::Prime, Some(dev)) => Some(HwFrames::drm_prime(dev, pixel, native.0, native.1)?),
                _ => None,
            };
            #[cfg(not(target_os = "linux"))]
            let frames: Option<HwFrames> = None;
            let input = (kind != SourceKind::Desktop).then(|| InputSpec {
                width: native.0,
                height: native.1,
                pix_fmt: if frames.is_some() { ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME } else { pixel },
                fps: s.quality.fps,
                hw_frames: frames.as_ref(),
            });
            // With DMA-BUF the device comes from the frames themselves (hwmap).
            let graph_device = if frames.is_some() { None } else { device.as_ref() };
            let graph = Graph::new(&(cand.chain)(&ctx), input.as_ref(), graph_device)?;
            let encoder = open_encoder(cand, &graph, &ctx, s.bitrate)?;
            let out = graph_out(&graph);
            Ok(Pipeline { graph, encoder, candidate: i, out, frames, _device: device })
        };
        match attempt() {
            Ok(p) => {
                tracing::info!(encoder = cand.encoder, label = cand.label, w = p.out.0, h = p.out.1, gpu = (kind == SourceKind::Prime), "encoder chosen");
                return Ok(p);
            }
            Err(e) => {
                tracing::info!(encoder = cand.encoder, "candidate rejected: {e}");
                // ENOSYS when opening NVENC = driver too old for its API.
                if cand.encoder == "h264_nvenc" && e.0.contains("Function not implemented") {
                    NVENC_DRIVER_TOO_OLD.store(true, Ordering::Relaxed);
                }
                last = e;
            }
        }
    }
    Err(last)
}

/// First GPU render node (the same one VAAPI opens by default).
#[cfg(target_os = "linux")]
fn render_node() -> String {
    (128..136).map(|n| format!("/dev/dri/renderD{n}")).find(|p| std::path::Path::new(p).exists()).unwrap_or_else(|| "/dev/dri/renderD128".into())
}

/// Rebuilds everything for `next`. The old pipeline goes first (NVENC
/// allows only a few sessions per GPU, and with the old one open the build
/// could settle for a worse encoder); if `next` fails, back to `current`.
fn rebuild(pipe: Pipeline, kind: SourceKind, native: (u32, u32), pixel: ff::AVPixelFormat, current: Settings, next: Settings) -> ffi::Result<(Pipeline, Settings)> {
    drop(pipe);
    match build(kind, native, pixel, &next, 0) {
        Ok(p) => Ok((p, next)),
        Err(e) => {
            tracing::warn!("new quality failed ({e}); keeping the previous one");
            Ok((build(kind, native, pixel, &current, 0)?, current))
        }
    }
}

/// Changes the encoder's bitrate and returns the one in effect. NVENC takes
/// it on the open session (FFmpeg's nvenc reconfigures at the next frame, with
/// an IDR); the others are closed and opened again, the old one first. If
/// the new one does not open, the old bitrate comes back.
fn set_bitrate(pipe: &mut Pipeline, native: (u32, u32), s: &Settings, bitrate: u32) -> ffi::Result<u32> {
    let cand = &encoders::candidates()[pipe.candidate];
    if cand.encoder == "h264_nvenc" {
        pipe.encoder.set_bitrate(bitrate);
        return Ok(bitrate);
    }
    let ctx = Ctx { native, out: pipe.out, fps: s.quality.fps };
    pipe.encoder.close();
    match open_encoder(cand, &pipe.graph, &ctx, bitrate) {
        Ok(e) => {
            pipe.encoder = e;
            Ok(bitrate)
        }
        Err(e) => {
            tracing::warn!(bitrate, "could not change the bitrate ({e}); keeping {}", s.bitrate);
            pipe.encoder = open_encoder(cand, &pipe.graph, &ctx, s.bitrate)?;
            Ok(s.bitrate)
        }
    }
}

fn graph_out(g: &Graph) -> (u32, u32) {
    g.out_size()
}

fn run(source: Source, quality: Quality, control: &Control, out: &broadcast::Sender<Arc<EncodedFrame>>) -> ffi::Result<()> {
    let (kind, mut cpu) = match source {
        Source::Cpu(s) => (SourceKind::Cpu, Some(s)),
        #[cfg(target_os = "windows")]
        Source::Desktop => (SourceKind::Desktop, None),
    };

    // Native size: from the first frame (CPU) or from the graph itself (Desktop).
    let mut first: Option<CpuFrame> = None;
    let (mut kind, mut native, mut pixel) = match cpu.as_mut() {
        Some(src) => loop {
            if control.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            if let Some(f) = src.next(Duration::from_millis(500)) {
                let n = (frame_kind(&f), (f.width, f.height), f.pixel.av());
                first = Some(f);
                break n;
            }
        },
        None => (kind, desktop_size(quality.fps)?, ff::AVPixelFormat::AV_PIX_FMT_BGRA),
    };
    control.stats.lock().unwrap().native = Some(native);

    // Sized for the real screen from the start: no encoder reopen a second in.
    let mut settings = Settings { quality, bitrate: quality.bitrate(native) };
    let mut pipe = build(kind, native, pixel, &settings, 0)?;
    let mut headers = h264::Headers::default();
    let mut captured_at: HashMap<i64, Instant> = HashMap::new();
    let mut pts: i64 = 0;
    let mut filtered = Frame::new();
    publish_encoder(control, &pipe, native);
    // Last image received, to re-encode while the screen is static.
    let mut last_input: Option<Frame> = None;
    let mut repeats = Repeats::new(Instant::now());
    let mut pacer = Pacer::new(settings.quality.fps);
    // A frame that came early, waiting for its turn (newer ones replace it).
    let mut early: Option<CpuFrame> = None;

    loop {
        if control.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        // Quality change: only the bitrate if that is all; everything if the size changed.
        // A failure keeps the stream going with what it had.
        if let Some(next) = control.pending.lock().unwrap().take() {
            if next != settings {
                let rebuild_all = next.quality.output_size(native) != settings.quality.output_size(native) || next.quality.fps != settings.quality.fps;
                if rebuild_all {
                    (pipe, settings) = rebuild(pipe, kind, native, pixel, settings, next)?;
                    last_input = None;
                } else {
                    settings.bitrate = set_bitrate(&mut pipe, native, &settings, next.bitrate)?;
                    settings.quality = next.quality;
                }
                pacer = Pacer::new(settings.quality.fps);
                control.request_key();
                publish_encoder(control, &pipe, native);
            }
        }

        // Input: CPU frame pushed into the graph, or the graph captures on its own.
        let mut got_input = true;
        if let Some(src) = cpu.as_mut() {
            let period = Duration::from_secs_f64(1.0 / settings.quality.fps.max(1) as f64);
            // Wakes up for the held frame's turn or for the next repeat, and at
            // least every 50 ms to see stop and quality requests.
            let wake = match (&early, pacer.due()) {
                (Some(_), Some(due)) => due,
                _ => repeats.next_at(period, control.force_key.load(Ordering::Relaxed)),
            };
            let timeout = wake.saturating_duration_since(Instant::now()).min(Duration::from_millis(50));
            let mut frame = match first.take() {
                Some(f) => Some(f),
                None => src.next(timeout),
            };
            let now = Instant::now();
            if let Some(f) = frame.take() {
                control.stats.lock().unwrap().captured += 1;
                // The compositor sends more than the chosen rate (GNOME: every
                // refresh): this one waits, and a newer one takes its place.
                if pacer.arrive(f.captured, now) {
                    frame = Some(f);
                    early = None;
                } else {
                    early = Some(f);
                }
            } else if early.is_some() && pacer.due().is_none_or(|d| now + pace::SLACK >= d) {
                // Nothing newer came in time: the held frame goes out.
                frame = early.take();
            }
            match frame {
                Some(f) => {
                    // The screen changed size (monitor or window switch) or
                    // PipeWire switched from DMA-BUF to memory: rebuild everything.
                    let shape = (frame_kind(&f), (f.width, f.height), f.pixel.av());
                    if shape != (kind, native, pixel) {
                        tracing::info!(w = f.width, h = f.height, gpu = f.data.on_gpu(), "capture changed; rebuilding video");
                        (kind, native, pixel) = shape;
                        control.stats.lock().unwrap().native = Some(native);
                        // The ideal bitrate for the new size (the session follows).
                        let next = Settings { quality: settings.quality, bitrate: settings.quality.bitrate(native) };
                        drop(pipe);
                        pipe = build(kind, native, pixel, &next, 0)?;
                        settings = next;
                        last_input = None;
                        control.request_key();
                        publish_encoder(control, &pipe, native);
                    }
                    pts += 1;
                    captured_at.insert(pts, f.captured);
                    let mut av = match f.data {
                        source::Pixels::Cpu(data) => Frame::from_cpu(data, f.width, f.height, f.stride, pixel, pts),
                        #[cfg(target_os = "linux")]
                        source::Pixels::DmaBuf { fd, offset, size, modifier } => {
                            let Some(frames) = pipe.frames.as_ref() else { continue };
                            Frame::from_dmabuf(fd, size, offset, f.stride, modifier, pixel, frames, native, pts)
                        }
                    };
                    last_input = av.share();
                    pacer.sent(now);
                    repeats.real(now);
                    pipe.graph.push(&mut av)?;
                }
                // The compositor only sends a frame when the screen changes. When static,
                // we re-encode the last image (like Sunshine): a few times right
                // after the change, so the encoder can refine what was blurry,
                // then slowly. A requested keyframe goes out right away.
                None => {
                    let due = early.is_none() && now >= repeats.next_at(period, control.force_key.load(Ordering::Relaxed));
                    match last_input.as_ref().filter(|_| due).and_then(Frame::share) {
                        Some(mut av) => {
                            pts += 1;
                            av.set_pts(pts);
                            repeats.repeated(now);
                            captured_at.insert(pts, now);
                            pipe.graph.push(&mut av)?;
                        }
                        None => got_input = false,
                    }
                }
            }
        }
        if !got_input {
            continue;
        }

        while pipe.graph.pull(&mut filtered)? {
            let t0 = Instant::now();
            if kind == SourceKind::Desktop {
                pts += 1;
                captured_at.insert(pts, t0);
                control.stats.lock().unwrap().captured += 1;
                unsafe { (*filtered.0).pts = pts };
            }
            let key = control.force_key.swap(false, Ordering::Relaxed);
            filtered.set_keyframe(key);
            pipe.encoder.send(&mut filtered)?;
            filtered.unref();
            while let Some(pkt) = pipe.encoder.receive()? {
                let captured = captured_at.remove(&pkt.pts).unwrap_or(t0);
                let ms = captured.elapsed().as_secs_f32() * 1000.0;
                {
                    let mut st = control.stats.lock().unwrap();
                    st.encoded += 1;
                    st.encode_ms = Some(st.encode_ms.map_or(ms, |p| p * 0.9 + ms * 0.1));
                }
                let data = headers.fix_keyframe(pkt.data, pkt.key);
                let Some(codec) = headers.codec() else { continue };
                let _ = out.send(Arc::new(EncodedFrame {
                    data: Bytes::from(data),
                    key: pkt.key,
                    codec,
                    width: pipe.out.0,
                    height: pipe.out.1,
                    captured,
                }));
            }
            if captured_at.len() > 240 {
                captured_at.clear();
            }
            // Windows capture (ddagrab) always has a new frame: without leaving
            // here on every frame, the loop never saw the stop request or the quality
            // change, and the old capture stayed attached to the screen (the next
            // stream could not open its own).
            if kind == SourceKind::Desktop {
                break;
            }
        }
    }
}

/// When the last image is re-encoded while the compositor sends nothing.
struct Repeats {
    /// Last frame that came from the source.
    real: Instant,
    /// Last frame pushed into the encoder (real or repeated).
    push: Instant,
    /// Repeats left at full rate after the last change.
    refine: u32,
}

/// Repeats right after a change, a period apart: the encoder spends them
/// sharpening what was blurry in motion (text especially).
const REFINE_FRAMES: u32 = 15;
/// After that, one every so often. It carries no new picture (a few hundred
/// bytes): it keeps the encoder refining and lets a loss show up at the viewer
/// without waiting for the next change. Chrome asks for a keyframe only after
/// 3 s without frames, and the site has no stall detection of its own.
const KEEPALIVE: Duration = Duration::from_millis(250);

impl Repeats {
    fn new(now: Instant) -> Self {
        Self { real: now, push: now, refine: 0 }
    }

    fn real(&mut self, now: Instant) {
        (self.real, self.push, self.refine) = (now, now, REFINE_FRAMES);
    }

    fn repeated(&mut self, now: Instant) {
        self.push = now;
        self.refine = self.refine.saturating_sub(1);
    }

    /// When the next repeat is due. Only after real silence: during motion a
    /// frame a little late is not a static screen, and repeating the old one
    /// would delay the new one behind it.
    fn next_at(&self, period: Duration, key: bool) -> Instant {
        let silence = self.real + period * 5 / 2;
        if key {
            // A keyframe request (someone joined) does not wait for the keepalive.
            (self.real + period * 3 / 2).max(self.push + period / 2)
        } else if self.refine > 0 {
            silence.max(self.push + period)
        } else {
            silence.max(self.push + KEEPALIVE)
        }
    }
}

pub(crate) mod pace {
    use std::time::{Duration, Instant};

    /// A frame up to this early still counts as on time.
    pub const SLACK: Duration = Duration::from_millis(1);

    /// Holds the source to the chosen frame rate. GNOME sends a frame on every
    /// refresh the screen changes (its own limit is the refresh rate, which
    /// the portal does not let us raise), so a 30 fps stream on a 60 Hz screen,
    /// or 60 on 144 Hz, is thinned here. When the source is not clearly faster
    /// than the target (60 on 60 Hz) every frame goes through as it arrives:
    /// holding one for a slot would only add delay.
    pub struct Pacer {
        period: Duration,
        /// Average gap between source frames while it sends continuously (s).
        gap: Option<f64>,
        last: Option<Instant>,
        due: Option<Instant>,
    }

    impl Pacer {
        pub fn new(fps: u32) -> Self {
            Self { period: Duration::from_secs_f64(1.0 / fps.max(1) as f64), gap: None, last: None, due: None }
        }

        fn thinning(&self) -> bool {
            self.gap.is_some_and(|g| g < self.period.as_secs_f64() * 0.85)
        }

        /// A frame captured at `at` arrived at `now`: `true` to send it now,
        /// `false` to hold it until [`due`](Self::due) (or a newer one).
        pub fn arrive(&mut self, at: Instant, now: Instant) -> bool {
            if let Some(prev) = self.last {
                let g = at.saturating_duration_since(prev).as_secs_f64();
                if g < 0.1 {
                    self.gap = Some(self.gap.map_or(g, |a| a * 0.9 + g * 0.1));
                }
            }
            self.last = Some(at);
            !self.thinning() || self.due.is_none_or(|d| now + SLACK >= d)
        }

        /// When the next frame may go out.
        pub fn due(&self) -> Option<Instant> {
            self.due
        }

        /// A frame went to the encoder at `now`. The slots stay on a fixed
        /// grid (so 144 Hz → 60 averages 60, not 48), never closer than half
        /// a period, and start over after a pause.
        pub fn sent(&mut self, now: Instant) {
            let p = self.period;
            self.due = Some(match self.due {
                Some(d) if now < d + p => (d + p).max(now + p / 2),
                _ => now + p,
            });
        }
    }
}

use pace::Pacer;

fn frame_kind(f: &CpuFrame) -> SourceKind {
    if f.data.on_gpu() { SourceKind::Prime } else { SourceKind::Cpu }
}

fn publish_encoder(control: &Control, pipe: &Pipeline, _native: (u32, u32)) {
    let cand = &encoders::candidates()[pipe.candidate];
    control.stats.lock().unwrap().encoder = Some(EncoderInfo {
        name: cand.label.to_owned(),
        hardware: cand.hardware,
        width: pipe.out.0,
        height: pipe.out.1,
    });
}

#[cfg(target_os = "windows")]
fn desktop_size(fps: u32) -> ffi::Result<(u32, u32)> {
    let g = Graph::new(&format!("ddagrab=output_idx=0:framerate={fps}"), None, None)?;
    Ok(g.out_size())
}

#[cfg(not(target_os = "windows"))]
fn desktop_size(_: u32) -> ffi::Result<(u32, u32)> {
    Err(ffi::Error("captura da área de trabalho só existe no Windows".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames from a source at `hz` (with ± `jitter_ms`) through the pacer at
    /// `fps`: how many go out, and the longest any waited for its slot (ms).
    fn simulate(hz: f64, jitter_ms: f64, fps: u32, secs: f64) -> (usize, usize, f64) {
        let base = Instant::now();
        let at = |t: f64| base + Duration::from_secs_f64(t);
        let mut p = Pacer::new(fps);
        let mut early: Option<f64> = None;
        let (mut sent, mut hold) = (0, 0f64);
        let n = (secs * hz) as usize;
        for k in 0..n {
            let j = ((k * 7919 % 13) as f64 / 12.0 - 0.5) * 2.0 * jitter_ms / 1000.0;
            let t = 0.01 + k as f64 / hz + j;
            // Nothing newer came before the slot: the held frame goes out then.
            if let (Some(e), Some(d)) = (early, p.due()) {
                let slot = d.duration_since(base).as_secs_f64();
                if slot <= t {
                    p.sent(d);
                    sent += 1;
                    hold = hold.max(slot - e);
                }
            }
            if p.arrive(at(t), at(t)) {
                p.sent(at(t));
                sent += 1;
                early = None;
            } else {
                early = Some(t);
            }
        }
        (n, sent, hold * 1000.0)
    }

    #[test]
    fn mesma_taxa_passa_tudo_sem_esperar() {
        // 60 Hz with jitter, 60 fps: every frame, none held.
        let (n, sent, hold) = simulate(60.0, 1.5, 60, 10.0);
        assert_eq!((sent, hold), (n, 0.0));
        // A screen a hair faster than the target does not drift into delay.
        let (n, sent, hold) = simulate(60.05, 0.5, 60, 30.0);
        assert_eq!((sent, hold), (n, 0.0));
    }

    #[test]
    fn afina_fonte_mais_rapida() {
        for (hz, fps) in [(60.0, 30), (144.0, 60), (120.0, 60), (75.0, 60), (240.0, 30)] {
            let (_, sent, hold) = simulate(hz, 1.0, fps, 10.0);
            let rate = sent as f64 / 10.0;
            assert!((rate - fps as f64).abs() <= fps as f64 * 0.04, "{hz} Hz → {fps}: {rate} fps");
            assert!(hold < 1000.0 / hz + 1.0, "{hz} Hz → {fps}: held {hold} ms");
        }
    }

    #[test]
    fn repete_so_depois_de_silencio() {
        let p = Duration::from_millis(16);
        let t0 = Instant::now();
        let mut r = Repeats::new(t0);
        r.real(t0);
        // In motion a frame 1 ms late is not silence.
        assert!(r.next_at(p, false) > t0 + p + Duration::from_millis(1));
        assert_eq!(r.next_at(p, false), t0 + p * 5 / 2);
        // Then a few a period apart, then the keepalive.
        let mut now = t0 + p * 5 / 2;
        for _ in 0..REFINE_FRAMES {
            assert_eq!(r.next_at(p, false), now);
            r.repeated(now);
            now += p;
        }
        assert_eq!(r.next_at(p, false), now - p + KEEPALIVE);
        // A keyframe request does not wait for the keepalive.
        assert!(r.next_at(p, true) <= now);
    }
}
