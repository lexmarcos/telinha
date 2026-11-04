//! Video pipeline: capture → filters (scale/GPU upload) → encoder.
//! Runs on its own thread and publishes H.264 frames ready to send.

pub mod encoders;
pub mod ffi;
pub mod h264;
pub mod h265;
pub mod source;
mod stage;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use ffmpeg_sys_next as ff;
use tokio::sync::broadcast;

use crate::capture::target::{self, DesktopOutput, Resize};
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

    /// Whether the cached keyframe is H.264 (all RTP can carry).
    pub fn is_h264(&self) -> bool {
        self.frames.first().is_some_and(|f| f.codec.starts_with("avc"))
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
    /// The source went away (a window closed): what to tell the person.
    pub ended: Option<String>,
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
    pub codec: Codec,
}

/// What the stream is encoded in. HEVC only goes to minimum delay viewers
/// whose browsers decode it in hardware (see the session); RTP is H.264.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Codec {
    #[default]
    H264,
    Hevc,
}

impl Codec {
    /// TELINHA_CODEC=hevc picks HEVC from the start (for --teste-video).
    pub fn from_env() -> Self {
        if std::env::var("TELINHA_CODEC").is_ok_and(|v| v.eq_ignore_ascii_case("hevc")) { Self::Hevc } else { Self::H264 }
    }
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
    /// A monitor, captured inside the pipeline's own graph.
    #[cfg(target_os = "windows")]
    Desktop(DesktopOutput),
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
    let options = encoders::options(cand, ctx);
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

/// What a pipeline needs to know about where its frames come from, beyond the
/// shape of each frame.
#[derive(Default)]
struct Origin {
    /// The monitor (with `SourceKind::Desktop`).
    output: DesktopOutput,
    /// The context of frames that arrive on the GPU (with `SourceKind::D3d11`).
    gpu_frames: Option<HwFrames>,
    /// Where the picture sits in its buffer, when that is larger than the
    /// picture (a window's DMA-BUF): the buffer's size and the picture's corner.
    layout: Layout,
}

/// See `Origin::layout`.
type Layout = Option<((u32, u32), (u32, u32))>;

fn layout_of(f: &CpuFrame) -> Layout {
    #[cfg(target_os = "linux")]
    if let source::Pixels::DmaBuf { buffer, corner, .. } = &f.data {
        return Some((*buffer, *corner));
    }
    let _ = f;
    None
}

impl Origin {
    /// The device `ddagrab` opens its monitor on: it only reaches the outputs of
    /// that one adapter, and by default it picks the first.
    fn desktop_device(&self) -> ffi::Result<Option<HwDevice>> {
        if self.output.adapter == 0 {
            return Ok(None);
        }
        HwDevice::create(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA, Some(&self.output.adapter.to_string())).map(Some)
    }
}

/// Tries candidates in order; the first that builds the graph and opens the
/// encoder wins. `start_at` skips the ones that already failed.
/// `pixel` is the pixel format (BGRX/RGBX), even when the pixels are on the GPU.
fn build(kind: SourceKind, native: (u32, u32), pixel: ff::AVPixelFormat, s: &Settings, start_at: usize, origin: &Origin) -> ffi::Result<(Pipeline, Encoder)> {
    let out = s.quality.output_size(native);
    let capture = (kind == SourceKind::Desktop).then(|| target::ddagrab_filter(origin.output, s.quality.fps));
    let corner = origin.layout.map_or((0, 0), |(_, corner)| corner);
    let ctx = Ctx { native, out, fps: s.quality.fps, capture, corner };
    let chains = if kind == SourceKind::D3d11 { SourceKind::Desktop } else { kind };
    let mut last = ffi::Error("nenhum codificador disponível".into());
    for (i, cand) in encoders::candidates().iter().enumerate().skip(start_at) {
        if cand.source != chains || cand.codec != s.codec {
            continue;
        }
        let attempt = || -> ffi::Result<(Pipeline, Encoder)> {
            let device = match (kind, cand.device) {
                #[cfg(target_os = "linux")]
                (SourceKind::Prime, _) => Some(HwDevice::create(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM, Some(&render_node()))?),
                (_, Some((t, d))) => Some(HwDevice::create(t, d)?),
                (SourceKind::Desktop, None) => origin.desktop_device()?,
                _ => None,
            };
            #[cfg(target_os = "linux")]
            let frames = match (kind, device.as_ref()) {
                (SourceKind::Prime, Some(dev)) => {
                    let (w, h) = origin.layout.map_or(native, |(buffer, _)| buffer);
                    Some(HwFrames::drm_prime(dev, pixel, w, h)?)
                }
                _ => None,
            };
            #[cfg(not(target_os = "linux"))]
            let frames: Option<HwFrames> = None;
            let (in_w, in_h) = origin.layout.filter(|_| kind == SourceKind::Prime).map_or(native, |(buffer, _)| buffer);
            let input = (kind != SourceKind::Desktop).then(|| InputSpec {
                width: in_w,
                height: in_h,
                pix_fmt: match kind {
                    SourceKind::D3d11 => ff::AVPixelFormat::AV_PIX_FMT_D3D11,
                    _ if frames.is_some() => ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME,
                    _ => pixel,
                },
                fps: s.quality.fps,
                hw_frames: frames.as_ref().or(origin.gpu_frames.as_ref()),
                // How gfxcapture labels its frames: told up front, like the graph
                // of a `ddagrab` knows it from the filter.
                color: if kind == SourceKind::D3d11 { D3D11_COLOR } else { "" },
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
                if cand.encoder.ends_with("_nvenc") && e.0.contains("Function not implemented") {
                    NVENC_DRIVER_TOO_OLD.store(true, Ordering::Relaxed);
                }
                last = e;
            }
        }
    }
    Err(last)
}

/// The `buffer` options that match gfxcapture's frames (sRGB, full range).
const D3D11_COLOR: &str = ":colorspace=gbr:range=pc";

/// First GPU render node (the same one VAAPI opens by default).
#[cfg(target_os = "linux")]
pub(crate) fn render_node() -> String {
    (128..136).map(|n| format!("/dev/dri/renderD{n}")).find(|p| std::path::Path::new(p).exists()).unwrap_or_else(|| "/dev/dri/renderD128".into())
}

/// Rebuilds everything for `next`. The old pipeline goes first (NVENC
/// allows only a few sessions per GPU, and with the old one open the build
/// could settle for a worse encoder); if `next` fails, back to `current`.
fn rebuild(pipe: Pipeline, stage: &EncodeStage, kind: SourceKind, native: (u32, u32), pixel: ff::AVPixelFormat, current: Settings, next: Settings, origin: &Origin) -> ffi::Result<(Pipeline, Settings)> {
    drop(stage.remove());
    drop(pipe);
    let (pipe, encoder, settings) = match build(kind, native, pixel, &next, 0, origin) {
        Ok((p, e)) => (p, e, next),
        Err(e) => {
            tracing::warn!("new quality failed ({e}); keeping the previous one");
            let (p, e) = build(kind, native, pixel, &current, 0, origin)?;
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
    if cand.encoder.ends_with("_nvenc") {
        encoder.set_bitrate(bitrate);
        return Ok(bitrate);
    }
    let ctx = Ctx { native, out: pipe.out, fps: s.quality.fps, capture: None, corner: (0, 0) };
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
    let mut origin = Origin::default();
    let (kind, mut cpu) = match source {
        Source::Cpu(s) => (SourceKind::Cpu, Some(s)),
        #[cfg(target_os = "windows")]
        Source::Desktop(output) => {
            origin.output = output;
            (SourceKind::Desktop, None)
        }
    };

    // Native size: from the first frame (CPU) or from the graph itself (Desktop).
    let mut first: Option<CpuFrame> = None;
    let (mut kind, mut native, mut pixel) = match cpu.as_mut() {
        Some(src) => loop {
            if control.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            if let Some(why) = src.ended() {
                control.stats.lock().unwrap().ended = Some(why);
                return Ok(());
            }
            if let Some(f) = src.next(Duration::from_millis(100)) {
                let n = (frame_kind(&f), (f.width, f.height), f.pixel.av());
                first = Some(f);
                break n;
            }
        },
        None => (kind, desktop_size(quality.fps, &origin)?, ff::AVPixelFormat::AV_PIX_FMT_BGRA),
    };
    let mut frames_ctx = first.as_ref().map_or(0, frame_context);
    follow_gpu_frames(&mut origin, first.as_ref());
    origin.layout = first.as_ref().and_then(layout_of);
    control.stats.lock().unwrap().native = Some(native);

    // Sized for the real screen from the start: no encoder reopen a second in.
    let mut settings = Settings { quality, bitrate: quality.bitrate(native), codec: Codec::from_env() };
    let (mut pipe, encoder) = build(kind, native, pixel, &settings, 0, &origin)?;
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
    // A window being dragged: the newest frame at the new size, until it settles.
    let mut resize = Resize::default();
    let mut resized: Option<CpuFrame> = None;

    loop {
        if control.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        // Quality change: only the bitrate if that is all; everything if the size changed.
        // A failure keeps the stream going with what it had.
        if let Some(next) = control.pending.lock().unwrap().take() {
            if next != settings {
                let rebuild_all = next.quality.output_size(native) != settings.quality.output_size(native)
                    || next.quality.fps != settings.quality.fps
                    || next.codec != settings.codec;
                if rebuild_all {
                    (pipe, settings) = rebuild(pipe, &stage, kind, native, pixel, settings, next, &origin)?;
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
            if let Some(why) = src.ended() {
                tracing::info!("capture source ended: {why}");
                control.stats.lock().unwrap().ended = Some(why);
                return Ok(());
            }
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
            // Only the size changed, as on every frame of a window being dragged:
            // that waits until it settles (the repeats below keep the viewers
            // going meanwhile, and a window that stops sends nothing more, so the
            // newest frame is held to go out then).
            if let Some(f) = frame.take() {
                let same_but_size = frame_kind(&f) == kind && f.pixel.av() == pixel && frame_context(&f) == frames_ctx;
                if same_but_size && ((f.width, f.height) != native || layout_of(&f) != origin.layout) {
                    resize.saw((f.width, f.height), now);
                    resized = Some(f);
                } else {
                    if same_but_size {
                        resize.clear();
                        resized = None;
                    }
                    frame = Some(f);
                }
            }
            if resize.due(now) {
                resize.clear();
                frame = resized.take().or(frame);
            }
            match frame {
                Some(f) => {
                    // The screen changed size (monitor or window switch) or
                    // PipeWire switched from DMA-BUF to memory: rebuild everything.
                    let shape = (frame_kind(&f), (f.width, f.height), f.pixel.av());
                    if shape != (kind, native, pixel) || frame_context(&f) != frames_ctx || layout_of(&f) != origin.layout {
                        tracing::info!(w = f.width, h = f.height, gpu = f.data.on_gpu(), "capture changed; rebuilding video");
                        (kind, native, pixel) = shape;
                        frames_ctx = frame_context(&f);
                        follow_gpu_frames(&mut origin, Some(&f));
                        origin.layout = layout_of(&f);
                        control.stats.lock().unwrap().native = Some(native);
                        // The ideal bitrate for the new size (the session follows).
                        let next = Settings { quality: settings.quality, bitrate: settings.quality.bitrate(native), codec: settings.codec };
                        drop(stage.remove());
                        drop(pipe);
                        let encoder;
                        (pipe, encoder) = build(kind, native, pixel, &next, 0, &origin)?;
                        stage.install(encoder);
                        settings = next;
                        // Read on the way out of the DMA-BUF arm below, the only one that can skip.
                        #[cfg_attr(target_os = "windows", allow(unused_assignments))]
                        {
                            last_input = None;
                        }
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
                        source::Pixels::DmaBuf { fd, offset, size, modifier, buffer, corner } => {
                            let Some(frames) = pipe.frames.as_ref() else { continue };
                            Frame::from_dmabuf(fd, size, offset, f.stride, modifier, pixel, frames, buffer, (corner, native), pts)
                        }
                        #[cfg(target_os = "windows")]
                        source::Pixels::D3d11(mut av) => {
                            av.set_pts(pts);
                            av
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
            let job = Job { frame, captured, converted: Instant::now(), size: pipe.out, codec: settings.codec };
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
    #[cfg(target_os = "windows")]
    if matches!(f.data, source::Pixels::D3d11(_)) {
        return SourceKind::D3d11;
    }
    if f.data.on_gpu() { SourceKind::Prime } else { SourceKind::Cpu }
}

/// Which GPU frames context a frame belongs to (0 for none). A window's capture
/// reopened at another size has its own device: a pipeline built for the old one
/// cannot use these frames, whatever their size.
fn frame_context(f: &CpuFrame) -> usize {
    #[cfg(target_os = "windows")]
    if let source::Pixels::D3d11(av) = &f.data {
        return unsafe { (*av.0).hw_frames_ctx.as_ref().map_or(0, |r| r.data as usize) };
    }
    let _ = f;
    0
}

/// The context the next pipeline takes its frames from.
fn follow_gpu_frames(origin: &mut Origin, f: Option<&CpuFrame>) {
    #[cfg(target_os = "windows")]
    if let Some(source::Pixels::D3d11(av)) = f.map(|f| &f.data) {
        origin.gpu_frames = HwFrames::of(av);
    }
    let _ = (origin, f);
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
fn desktop_size(fps: u32, origin: &Origin) -> ffi::Result<(u32, u32)> {
    let device = origin.desktop_device()?;
    let g = Graph::new(&target::ddagrab_filter(origin.output, fps), None, device.as_ref())?;
    Ok(g.out_size())
}

#[cfg(not(target_os = "windows"))]
fn desktop_size(_: u32, _: &Origin) -> ffi::Result<(u32, u32)> {
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

    /// A source like a captured window: frames only when it wants, any size.
    struct Janela {
        /// (when to send, from the start; size)
        frames: std::collections::VecDeque<(Duration, (u32, u32))>,
        start: Instant,
        ended_at: Option<Duration>,
        dropped: Arc<AtomicBool>,
    }

    impl Janela {
        fn new(frames: Vec<(u64, (u32, u32))>, ended_at: Option<u64>) -> (Self, Arc<AtomicBool>) {
            let dropped = Arc::new(AtomicBool::new(false));
            let frames = frames.into_iter().map(|(ms, size)| (Duration::from_millis(ms), size)).collect();
            (Self { frames, start: Instant::now(), ended_at: ended_at.map(Duration::from_millis), dropped: dropped.clone() }, dropped)
        }
    }

    impl CpuSource for Janela {
        fn next(&mut self, timeout: Duration) -> Option<CpuFrame> {
            let elapsed = self.start.elapsed();
            let due = self.frames.front().is_some_and(|(at, _)| *at <= elapsed);
            if !due {
                std::thread::sleep(timeout.min(Duration::from_millis(5)));
                return None;
            }
            let (_, (width, height)) = self.frames.pop_front()?;
            let data = source::Pixels::Cpu(vec![128; (width * height * 4) as usize]);
            Some(CpuFrame { data, width, height, stride: width * 4, pixel: source::PixelFormat::Bgrx, captured: Instant::now() })
        }

        fn ended(&self) -> Option<String> {
            self.ended_at.filter(|at| self.start.elapsed() >= *at).map(|_| "fechou".to_owned())
        }
    }

    impl Drop for Janela {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Relaxed);
        }
    }

    /// Everything that came out in `ms`, keeping an eye on the thread's error.
    /// `None` when this machine has no encoder to try it on.
    fn saida(control: &Control, rx: &mut broadcast::Receiver<Arc<EncodedFrame>>, ms: u64) -> Option<Vec<Arc<EncodedFrame>>> {
        let until = Instant::now() + Duration::from_millis(ms);
        let mut out = Vec::new();
        while Instant::now() < until {
            match rx.try_recv() {
                Ok(f) => out.push(f),
                Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(2)),
            }
            if let Some(e) = control.stats().error {
                eprintln!("no encoder here, skipping: {e}");
                return None;
            }
        }
        Some(out)
    }

    fn espera(what: &AtomicBool, ms: u64) -> bool {
        let until = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < until {
            if what.load(Ordering::Relaxed) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn qualidade() -> Quality {
        Quality { fps: 30, ..Quality::default() }
    }

    #[test]
    fn janela_parada_continua_viva_e_para_quando_pedem() {
        let (janela, dropped) = Janela::new(vec![(0, (320, 240)), (30, (320, 240))], None);
        let (tx, mut rx) = broadcast::channel(256);
        let control = start(Source::Cpu(Box::new(janela)), qualidade(), tx);
        // Nothing new from the source after the first moments: viewers still get
        // repeats, without which they would freeze.
        let Some(out) = saida(&control, &mut rx, 1500) else { return };
        assert!(out.len() >= 5, "only {} frames from a still window", out.len());
        let tail = saida(&control, &mut rx, 800).unwrap_or_default();
        assert!(!tail.is_empty(), "the repeats stopped");
        // Someone joins: a keyframe, with no new image from the window.
        control.request_key();
        let after = saida(&control, &mut rx, 800).unwrap_or_default();
        assert!(after.iter().any(|f| f.key), "no keyframe from a still window");
        // The stop request is seen with no frame to wake the loop.
        control.stop.store(true, Ordering::Relaxed);
        assert!(espera(&dropped, 1000), "the video thread did not stop");
    }

    #[test]
    fn janela_fechada_encerra_com_aviso() {
        let (janela, dropped) = Janela::new(vec![(0, (320, 240))], Some(300));
        let (tx, mut rx) = broadcast::channel(256);
        let control = start(Source::Cpu(Box::new(janela)), qualidade(), tx);
        if saida(&control, &mut rx, 200).is_none() {
            return;
        }
        assert!(espera(&dropped, 1000), "the video thread did not end");
        assert_eq!(control.stats().ended.as_deref(), Some("fechou"));
        assert_eq!(control.stats().error, None, "ending is not a failure");
    }

    #[test]
    fn janela_que_fecha_antes_do_primeiro_quadro_tambem_avisa() {
        let (janela, dropped) = Janela::new(vec![], Some(50));
        let (tx, _rx) = broadcast::channel(16);
        let control = start(Source::Cpu(Box::new(janela)), qualidade(), tx);
        assert!(espera(&dropped, 1000));
        assert_eq!(control.stats().ended.as_deref(), Some("fechou"));
    }

    #[test]
    fn arrastar_a_janela_nao_reabre_a_cada_pixel() {
        // A size per frame for 600 ms, odd ones included, then the window stops:
        // no frame comes after the last size.
        let mut frames = vec![(0, (320, 240))];
        frames.extend((0..40u32).map(|k| (100 + k as u64 * 15, (321 + k * 3, 241 + k))));
        let last = *frames.last().unwrap();
        let (janela, _dropped) = Janela::new(frames, None);
        let (tx, mut rx) = broadcast::channel(256);
        let control = start(Source::Cpu(Box::new(janela)), qualidade(), tx);
        let Some(out) = saida(&control, &mut rx, 1600) else { return };
        let mut sizes: Vec<(u32, u32)> = Vec::new();
        for f in &out {
            if sizes.last() != Some(&(f.width, f.height)) {
                sizes.push((f.width, f.height));
            }
        }
        let wanted = qualidade().output_size(last.1);
        assert_eq!(sizes, [(320, 240), wanted], "reopened once, for the size it settled on");
        assert!(sizes.iter().all(|(w, h)| w % 2 == 0 && h % 2 == 0));
        assert_eq!(control.stats().native, Some(last.1));
        control.stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn janela_minuscula_e_de_tamanho_impar_abre_o_codificador() {
        let (janela, _dropped) = Janela::new(vec![(0, (37, 21)), (40, (37, 21))], None);
        let (tx, mut rx) = broadcast::channel(64);
        let control = start(Source::Cpu(Box::new(janela)), qualidade(), tx);
        let Some(out) = saida(&control, &mut rx, 800) else { return };
        let f = out.first().expect("a frame");
        assert_eq!((f.width, f.height), qualidade().output_size((37, 21)));
        control.stop.store(true, Ordering::Relaxed);
    }

    /// The options a window's frames are announced with have to exist in the
    /// `buffer` filter: on Windows a wrong name would fail every candidate at once.
    #[test]
    fn opcoes_de_cor_da_entrada_existem() {
        let input = InputSpec { width: 64, height: 64, pix_fmt: ff::AVPixelFormat::AV_PIX_FMT_BGR0, fps: 30, hw_frames: None, color: D3D11_COLOR };
        assert!(Graph::new("", Some(&input), None).is_ok());
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
