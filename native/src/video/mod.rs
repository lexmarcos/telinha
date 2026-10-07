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
pub const NVENC_MIN_DRIVER: &str = "531";

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
/// requests keyframes and stops.
pub fn start(source: Source, settings: Settings, out: broadcast::Sender<Arc<EncodedFrame>>) -> Arc<Control> {
    ffi::silence_logs();
    let control = Arc::new(Control::default());
    let c = control.clone();
    std::thread::Builder::new()
        .name("telinha-video".into())
        .spawn(move || {
            if let Err(e) = run(source, settings, &c, &out) {
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

fn graph_out(g: &Graph) -> (u32, u32) {
    g.out_size()
}

fn run(source: Source, mut settings: Settings, control: &Control, out: &broadcast::Sender<Arc<EncodedFrame>>) -> ffi::Result<()> {
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
        None => (kind, desktop_size(settings.quality.fps)?, ff::AVPixelFormat::AV_PIX_FMT_BGRA),
    };
    control.stats.lock().unwrap().native = Some(native);

    let mut pipe = build(kind, native, pixel, &settings, 0)?;
    let mut headers = h264::Headers::default();
    let mut captured_at: HashMap<i64, Instant> = HashMap::new();
    let mut pts: i64 = 0;
    let mut filtered = Frame::new();
    publish_encoder(control, &pipe, native);
    // Last image received, to re-encode while the screen is static.
    let mut last_input: Option<Frame> = None;
    let mut last_change = Instant::now();
    let mut last_push = Instant::now();

    loop {
        if control.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        // Quality change: only the encoder if just the bitrate changed; everything if the size changed.
        if let Some(next) = control.pending.lock().unwrap().take() {
            if next != settings {
                let rebuild_all = next.quality.output_size(native) != settings.quality.output_size(native) || next.quality.fps != settings.quality.fps;
                settings = next;
                if rebuild_all {
                    pipe = build(kind, native, pixel, &settings, 0)?;
                    last_input = None;
                } else {
                    let cand = &encoders::candidates()[pipe.candidate];
                    let ctx = Ctx { native, out: pipe.out, fps: settings.quality.fps };
                    pipe.encoder = open_encoder(cand, &pipe.graph, &ctx, settings.bitrate)?;
                }
                control.request_key();
                publish_encoder(control, &pipe, native);
            }
        }

        // Input: CPU frame pushed into the graph, or the graph captures on its own.
        let mut got_input = true;
        if let Some(src) = cpu.as_mut() {
            let period = Duration::from_secs_f64(1.0 / settings.quality.fps.max(1) as f64);
            let frame = match first.take() {
                Some(f) => Some(f),
                None => src.next(period),
            };
            match frame {
                Some(f) => {
                    // The screen changed size (monitor or window switch) or
                    // PipeWire switched from DMA-BUF to memory: rebuild everything.
                    let shape = (frame_kind(&f), (f.width, f.height), f.pixel.av());
                    if shape != (kind, native, pixel) {
                        tracing::info!(w = f.width, h = f.height, gpu = f.data.on_gpu(), "capture changed; rebuilding video");
                        (kind, native, pixel) = shape;
                        control.stats.lock().unwrap().native = Some(native);
                        pipe = build(kind, native, pixel, &settings, 0)?;
                        last_input = None;
                        control.request_key();
                        publish_encoder(control, &pipe, native);
                    }
                    pts += 1;
                    captured_at.insert(pts, f.captured);
                    control.stats.lock().unwrap().captured += 1;
                    let mut av = match f.data {
                        source::Pixels::Cpu(data) => Frame::from_cpu(data, f.width, f.height, f.stride, pixel, pts),
                        #[cfg(target_os = "linux")]
                        source::Pixels::DmaBuf { fd, offset, size, modifier } => {
                            let Some(frames) = pipe.frames.as_ref() else { continue };
                            Frame::from_dmabuf(fd, size, offset, f.stride, modifier, pixel, frames, native, pts)
                        }
                    };
                    last_input = av.share();
                    last_change = Instant::now();
                    last_push = last_change;
                    pipe.graph.push(&mut av)?;
                }
                // The compositor only sends a frame when the screen changes. When static,
                // we re-encode the last image (like Sunshine): at full rate
                // right after the change, so the encoder can refine what
                // was blurry, and slowly afterwards. A requested keyframe goes out immediately.
                None => {
                    let due = last_change.elapsed() < REFINE
                        || last_push.elapsed() >= KEEPALIVE
                        || control.force_key.load(Ordering::Relaxed);
                    match last_input.as_ref().filter(|_| due).and_then(Frame::share) {
                        Some(mut av) => {
                            pts += 1;
                            av.set_pts(pts);
                            last_push = Instant::now();
                            captured_at.insert(pts, last_push);
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

/// How long after a change a static screen is still re-encoded at full rate.
const REFINE: Duration = Duration::from_secs(3);
/// After that, how often.
const KEEPALIVE: Duration = Duration::from_millis(100);

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
