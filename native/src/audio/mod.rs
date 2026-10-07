//! Computer audio: captures the sound output, encodes it as Opus (stereo,
//! 10 ms frames, low-delay mode and constant bitrate, like Sunshine)
//! and publishes the packets to the connections.
//!
//! Linux: each app's audio through PipeWire, except Discord.
//! Windows: WASAPI loopback, without Discord when it is open (so people
//! in the call do not hear their own voice back).

pub mod chime;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

use std::ffi::CString;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use bytes::Bytes;
use ffmpeg_sys_next as ff;
use tokio::sync::broadcast;

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
const BITRATE: i64 = 160_000;

#[derive(Debug)]
pub struct OpusPacket {
    pub data: Bytes,
    pub duration: Duration,
}

pub struct Audio {
    stop: Arc<AtomicBool>,
    pub packets: broadcast::Sender<Arc<OpusPacket>>,
    /// Description for the log ("all audio except Discord").
    pub source: String,
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub fn start() -> Result<Audio, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (packets, _) = broadcast::channel(64);
    let (tx, rx) = mpsc::sync_channel::<Vec<f32>>(64);

    #[cfg(target_os = "linux")]
    let source = linux::start(tx, stop.clone())?;
    #[cfg(target_os = "windows")]
    let source = windows::start(tx, stop.clone())?;
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let source = {
        drop(tx);
        return Err("captura de som não existe neste sistema".into());
    };

    let mut encoder = Opus::open()?;
    let (out, s) = (packets.clone(), stop.clone());
    std::thread::Builder::new()
        .name("telinha-opus".into())
        .spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(samples) => {
                        for p in encoder.push(&samples) {
                            let _ = out.send(Arc::new(p));
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .map_err(|e| e.to_string())?;
    tracing::info!(fonte = %source, "computer audio on");
    Ok(Audio { stop, packets, source })
}

/// Records `secs` seconds of audio, without encoding (for tests).
pub fn record(secs: u64) -> Result<Vec<f32>, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::sync_channel::<Vec<f32>>(1024);
    #[cfg(target_os = "linux")]
    linux::start(tx, stop.clone())?;
    #[cfg(target_os = "windows")]
    windows::start(tx, stop.clone())?;
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    drop(tx);
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    let mut out = Vec::new();
    while std::time::Instant::now() < deadline {
        if let Ok(s) = rx.recv_timeout(Duration::from_millis(100)) {
            out.extend(s);
        }
    }
    stop.store(true, Ordering::Relaxed);
    Ok(out)
}

/* ---------------- Opus via FFmpeg (libopus) ---------------- */

struct Opus {
    ctx: *mut ff::AVCodecContext,
    frame: *mut ff::AVFrame,
    pkt: *mut ff::AVPacket,
    frame_size: usize,
    pending: Vec<f32>,
    pts: i64,
}

unsafe impl Send for Opus {}

impl Opus {
    fn open() -> Result<Self, String> {
        unsafe {
            let codec = ff::avcodec_find_encoder_by_name(c"libopus".as_ptr());
            if codec.is_null() {
                return Err("Opus não existe nesta build do FFmpeg".into());
            }
            let ctx = ff::avcodec_alloc_context3(codec);
            let c = &mut *ctx;
            c.sample_fmt = ff::AVSampleFormat::AV_SAMPLE_FMT_FLT;
            c.sample_rate = RATE as i32;
            ff::av_channel_layout_default(&mut c.ch_layout, CHANNELS as i32);
            c.bit_rate = BITRATE;
            c.time_base = ff::AVRational { num: 1, den: RATE as i32 };
            for (k, v) in [("application", "lowdelay"), ("frame_duration", "10"), ("vbr", "off")] {
                let (k, v) = (CString::new(k).unwrap(), CString::new(v).unwrap());
                ff::av_opt_set(c.priv_data, k.as_ptr(), v.as_ptr(), 0);
            }
            let ret = ff::avcodec_open2(ctx, codec, ptr::null_mut());
            if ret < 0 {
                return Err(format!("abrir o Opus: {}", crate::video::ffi::av_err(ret)));
            }
            let frame_size = (*ctx).frame_size.max(480) as usize;
            Ok(Self { ctx, frame: ff::av_frame_alloc(), pkt: ff::av_packet_alloc(), frame_size, pending: Vec::new(), pts: 0 })
        }
    }

    /// Buffers samples and returns the Opus packets for each complete frame.
    fn push(&mut self, samples: &[f32]) -> Vec<OpusPacket> {
        self.pending.extend_from_slice(samples);
        let per = self.frame_size * CHANNELS;
        let mut out = Vec::new();
        while self.pending.len() >= per {
            let chunk: Vec<f32> = self.pending.drain(..per).collect();
            unsafe {
                let f = &mut *self.frame;
                f.nb_samples = self.frame_size as i32;
                f.format = ff::AVSampleFormat::AV_SAMPLE_FMT_FLT as i32;
                f.sample_rate = RATE as i32;
                ff::av_channel_layout_default(&mut f.ch_layout, CHANNELS as i32);
                if ff::av_frame_get_buffer(self.frame, 0) < 0 {
                    break;
                }
                ptr::copy_nonoverlapping(chunk.as_ptr() as *const u8, f.data[0], chunk.len() * 4);
                f.pts = self.pts;
                self.pts += self.frame_size as i64;
                if ff::avcodec_send_frame(self.ctx, self.frame) >= 0 {
                    while ff::avcodec_receive_packet(self.ctx, self.pkt) >= 0 {
                        let p = &*self.pkt;
                        out.push(OpusPacket {
                            data: Bytes::copy_from_slice(std::slice::from_raw_parts(p.data, p.size as usize)),
                            duration: Duration::from_micros(self.frame_size as u64 * 1_000_000 / RATE as u64),
                        });
                        ff::av_packet_unref(self.pkt);
                    }
                }
                ff::av_frame_unref(self.frame);
            }
        }
        out
    }
}

impl Drop for Opus {
    fn drop(&mut self) {
        unsafe {
            ff::av_frame_free(&mut self.frame);
            ff::av_packet_free(&mut self.pkt);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}
