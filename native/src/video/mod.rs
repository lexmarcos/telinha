//! Video pipeline: capture → filters (scale/GPU upload) → encoder.
//! Runs on its own thread and publishes H.264 frames ready to send.

pub mod encoders;
pub mod ffi;
pub mod h264;
pub mod source;
mod stage;

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
use stage::{EncodeStage, Job, Overlap};

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
    /// Position in the stream (counts every frame out of the encoder).
    pub index: u64,
}

/// The last keyframe and every frame since, so a viewer who joins can start
/// from it instead of making the one encoder send everyone a new keyframe.
/// Dropped once it grows past what is worth replaying (motion makes it grow
/// fast; a still screen only adds tiny repeats).
#[derive(Default)]
pub struct Gop {
    frames: Vec<Arc<EncodedFrame>>,
    /// Bytes after the keyframe.
    rest: usize,
}

const GOP_MAX_BYTES: usize = 8 << 20;
const GOP_MAX_FRAMES: usize = 600;

impl Gop {
    fn push(&mut self, f: &Arc<EncodedFrame>) {
        if f.key {
            self.frames.clear();
            self.rest = 0;
        } else if self.frames.is_empty() {
            return;
        } else {
            self.rest += f.data.len();
        }
        self.frames.push(f.clone());
        if self.rest > GOP_MAX_BYTES || self.frames.len() > GOP_MAX_FRAMES {
            self.clear();
        }
    }

    fn clear(&mut self) {
        self.frames.clear();
        self.rest = 0;
    }

    /// The cached frames, keyframe first, if `fits(keyframe bytes, bytes
    /// after it)` says replaying them is cheap enough.
    pub fn replay(&self, fits: impl Fn(usize, usize) -> bool) -> Option<Vec<Arc<EncodedFrame>>> {
        let key = self.frames.first()?;
        fits(key.data.len(), self.rest).then(|| self.frames.clone())
    }
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

/// How long each stage took per frame (ms) since the last read, for the
/// "video rates" log: waiting (capture to the pipeline: the mailbox and the
/// pacer), converting (upload, scale, colour), encoding (into the encoder to
/// the packet out) and in all (capture to packet).
#[derive(Default, Debug)]
pub struct Timings {
    pub wait: Vec<f32>,
    pub convert: Vec<f32>,
    pub encode: Vec<f32>,
    pub total: Vec<f32>,
}

impl Timings {
    fn add(v: &mut Vec<f32>, d: Duration) {
        if v.len() < 4096 {
            v.push(d.as_secs_f32() * 1000.0);
        }
    }
}

/// "p50/p95/max" in ms, or "-" without samples.
pub fn percentiles(v: &mut [f32]) -> String {
    if v.is_empty() {
        return "-".into();
    }
    v.sort_by(f32::total_cmp);
    let at = |p: f32| v[((v.len() - 1) as f32 * p).round() as usize];
    format!("{:.1}/{:.1}/{:.1}", at(0.5), at(0.95), v[v.len() - 1])
}

/// Commands for the video thread, read on every frame.
#[derive(Default)]
pub struct Control {
    pub force_key: AtomicBool,
    pub stop: AtomicBool,
    pending: Mutex<Option<Settings>>,
    pub stats: Mutex<Stats>,
    pub gop: Mutex<Gop>,
    pub timings: Mutex<Timings>,
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

    pub fn take_timings(&self) -> Timings {
        std::mem::take(&mut *self.timings.lock().unwrap())
    }

    /// See [`Gop::replay`].
    pub fn replay(&self, fits: impl Fn(usize, usize) -> bool) -> Option<Vec<Arc<EncodedFrame>>> {
        self.gop.lock().unwrap().replay(fits)
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
            if let Err(e) = run(source, quality, &c, out) {
                tracing::error!("video stopped: {e}");
                c.stats.lock().unwrap().error = Some(e.0);
            }
        })
        .expect("video thread");
    control
}

/// Capture-side half of the pipeline; the encoder lives in the
/// [`EncodeStage`].
struct Pipeline {
    graph: Graph,
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
fn build(kind: SourceKind, native: (u32, u32), pixel: ff::AVPixelFormat, s: &Settings, start_at: usize) -> ffi::Result<(Pipeline, Encoder)> {
    let out = s.quality.output_size(native);
    let ctx = Ctx { native, out, fps: s.quality.fps };
    let mut last = ffi::Error("nenhum codificador disponível".into());
    for (i, cand) in encoders::candidates().iter().enumerate().skip(start_at) {
        if cand.source != kind {
            continue;
        }
        let attempt = || -> ffi::Result<(Pipeline, Encoder)> {
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
            Ok((Pipeline { graph, candidate: i, out, frames, _device: device }, encoder))
        };
        match attempt() {
            Ok(p) => {
                tracing::info!(encoder = cand.encoder, label = cand.label, w = p.0.out.0, h = p.0.out.1, gpu = (kind == SourceKind::Prime), "encoder chosen");
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
pub(crate) fn render_node() -> String {
    (128..136).map(|n| format!("/dev/dri/renderD{n}")).find(|p| std::path::Path::new(p).exists()).unwrap_or_else(|| "/dev/dri/renderD128".into())
}

/// Rebuilds everything for `next`. The old pipeline goes first (NVENC
/// allows only a few sessions per GPU, and with the old one open the build
/// could settle for a worse encoder); if `next` fails, back to `current`.
fn rebuild(pipe: Pipeline, stage: &EncodeStage, kind: SourceKind, native: (u32, u32), pixel: ff::AVPixelFormat, current: Settings, next: Settings) -> ffi::Result<(Pipeline, Settings)> {
    drop(stage.remove());
    drop(pipe);
    let (pipe, encoder, settings) = match build(kind, native, pixel, &next, 0) {
        Ok((p, e)) => (p, e, next),
        Err(e) => {
            tracing::warn!("new quality failed ({e}); keeping the previous one");
            let (p, e) = build(kind, native, pixel, &current, 0)?;
            (p, e, current)
        }
    };
    stage.install(encoder);
    Ok((pipe, settings))
}

/// Changes the encoder's bitrate and returns the one in effect. NVENC takes
/// it on the open session (FFmpeg's nvenc reconfigures at the next frame, with
/// an IDR); the others are closed and opened again, the old one first. If
/// the new one does not open, the old bitrate comes back.
fn set_bitrate(pipe: &Pipeline, encoder: &mut Encoder, native: (u32, u32), s: &Settings, bitrate: u32) -> ffi::Result<u32> {
    let cand = &encoders::candidates()[pipe.candidate];
    if cand.encoder == "h264_nvenc" {
        encoder.set_bitrate(bitrate);
        return Ok(bitrate);
    }
    let ctx = Ctx { native, out: pipe.out, fps: s.quality.fps };
    encoder.close();
    match open_encoder(cand, &pipe.graph, &ctx, bitrate) {
        Ok(e) => {
            *encoder = e;
            Ok(bitrate)
        }
        Err(e) => {
            tracing::warn!(bitrate, "could not change the bitrate ({e}); keeping {}", s.bitrate);
            *encoder = open_encoder(cand, &pipe.graph, &ctx, s.bitrate)?;
            Ok(s.bitrate)
        }
    }
}

fn graph_out(g: &Graph) -> (u32, u32) {
    g.out_size()
}

fn run(source: Source, quality: Quality, control: &Arc<Control>, out: broadcast::Sender<Arc<EncodedFrame>>) -> ffi::Result<()> {
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
    let (mut pipe, encoder) = build(kind, native, pixel, &settings, 0)?;
    // Declared after the pipeline so it goes first: its thread stops and the
    // encoder closes before the graph and devices do.
    let stage = EncodeStage::start(control.clone(), out.clone());
    stage.install(encoder);
    let mut overlap = Overlap::new();
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
    // When the last input went into the graph (for the conversion time).
    let mut convert_from: Option<Instant> = None;

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
                    (pipe, settings) = rebuild(pipe, &stage, kind, native, pixel, settings, next)?;
                    last_input = None;
                } else {
                    if let Some(encoder) = stage.encoder().as_mut() {
                        settings.bitrate = set_bitrate(&pipe, encoder, native, &settings, next.bitrate)?;
                    }
                    settings.quality = next.quality;
                }
                pacer = Pacer::new(settings.quality.fps);
                // Frames from before the change do not lead into the ones after it.
                control.gop.lock().unwrap().clear();
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
                        drop(stage.remove());
                        drop(pipe);
                        let encoder;
                        (pipe, encoder) = build(kind, native, pixel, &next, 0)?;
                        stage.install(encoder);
                        settings = next;
                        last_input = None;
                        control.gop.lock().unwrap().clear();
                        control.request_key();
                        publish_encoder(control, &pipe, native);
                    }
                    pts += 1;
                    captured_at.insert(pts, f.captured);
                    Timings::add(&mut control.timings.lock().unwrap().wait, now.saturating_duration_since(f.captured));
                    convert_from = Some(Instant::now());
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
                            convert_from = Some(Instant::now());
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
            let mut convert_ms = 0.0;
            if let Some(from) = convert_from.take() {
                Timings::add(&mut control.timings.lock().unwrap().convert, t0 - from);
                convert_ms = (t0 - from).as_secs_f32() * 1000.0;
            }
            if kind == SourceKind::Desktop {
                pts += 1;
                captured_at.insert(pts, t0);
                control.stats.lock().unwrap().captured += 1;
                unsafe { (*filtered.0).pts = pts };
            }
            let captured = captured_at.remove(&filtered.pts()).unwrap_or(t0);
            let frame = std::mem::replace(&mut filtered, Frame::new());
            let job = Job { frame, captured, converted: Instant::now(), size: pipe.out };
            let period_ms = 1000.0 / settings.quality.fps.max(1) as f32;
            let encoded = control.stats.lock().unwrap().encoded;
            if overlap.update(t0, convert_ms, stage.encode_ms(), period_ms, encoded) {
                // To the encoder's thread; one it had not started yet is skipped.
                if stage.submit(job) {
                    control.stats.lock().unwrap().skipped += 1;
                }
            } else {
                stage.encode_here(job, control, &out)?;
            }
            if let Some(e) = stage.failure() {
                return Err(e);
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
    fn guarda_o_gop_para_quem_chega() {
        let f = |index: u64, key: bool, len: usize| {
            Arc::new(EncodedFrame { data: Bytes::from(vec![0u8; len]), key, codec: "avc1.640028".into(), width: 2, height: 2, captured: Instant::now(), index })
        };
        let mut g = Gop::default();
        // Nothing before the first keyframe.
        g.push(&f(1, false, 10));
        assert!(g.replay(|_, _| true).is_none());
        g.push(&f(2, true, 1000));
        g.push(&f(3, false, 10));
        g.push(&f(4, false, 20));
        let r = g.replay(|key, rest| key == 1000 && rest == 30).unwrap();
        assert_eq!(r.iter().map(|f| f.index).collect::<Vec<_>>(), [2, 3, 4]);
        // Too much after the keyframe for this viewer: not replayed.
        assert!(g.replay(|_, rest| rest <= 20).is_none());
        // A new keyframe starts over.
        g.push(&f(5, true, 900));
        assert_eq!(g.replay(|_, rest| rest == 0).unwrap().len(), 1);
        // Past the limits it is dropped until the next keyframe.
        g.push(&f(6, false, GOP_MAX_BYTES + 1));
        g.push(&f(7, false, 10));
        assert!(g.replay(|_, _| true).is_none());
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
