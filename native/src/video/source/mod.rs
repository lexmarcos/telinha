//! Image sources: PipeWire on Linux (on the GPU via DMA-BUF or on the CPU) and
//! the test pattern.

pub mod test;

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgrx,
    Rgbx,
}

impl PixelFormat {
    pub fn av(self) -> ffmpeg_sys_next::AVPixelFormat {
        match self {
            Self::Bgrx => ffmpeg_sys_next::AVPixelFormat::AV_PIX_FMT_BGR0,
            Self::Rgbx => ffmpeg_sys_next::AVPixelFormat::AV_PIX_FMT_RGB0,
        }
    }
}

/// Where the image lives.
pub enum Pixels {
    /// In memory, 4 bytes per pixel.
    Cpu(Vec<u8>),
    /// On the GPU (Linux): a DMA-BUF buffer from the compositor, with no copy at all.
    #[cfg(target_os = "linux")]
    DmaBuf { fd: std::os::fd::OwnedFd, offset: u32, size: u32, modifier: u64 },
}

impl Pixels {
    pub fn on_gpu(&self) -> bool {
        !matches!(self, Self::Cpu(_))
    }
}

pub struct CpuFrame {
    /// 4-bytes-per-pixel image, in `pixel` order.
    pub data: Pixels,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel: PixelFormat,
    pub captured: Instant,
}

pub trait CpuSource: Send {
    /// Next frame, waiting at most `timeout`.
    fn next(&mut self, timeout: Duration) -> Option<CpuFrame>;
}

/// Frame buffers in memory, reused: a 4K frame is 33 MB, and a fresh one
/// every frame meant the kernel mapping and zeroing ~8000 pages each time
/// (and unmapping them when FFmpeg let go).
static POOL: std::sync::Mutex<Vec<Vec<u8>>> = std::sync::Mutex::new(Vec::new());
const POOL_SIZE: usize = 4;

/// An empty buffer with room for `len` bytes, from the pool when it has one.
pub fn buffer(len: usize) -> Vec<u8> {
    let reused = POOL.lock().unwrap().pop();
    match reused {
        Some(mut v) if v.capacity() >= len && v.capacity() <= len * 2 => {
            v.clear();
            v
        }
        _ => Vec::with_capacity(len),
    }
}

/// Gives a buffer back for [`buffer`] to hand out again.
pub fn recycle(v: Vec<u8>) {
    if v.capacity() == 0 {
        return;
    }
    let mut pool = POOL.lock().unwrap();
    if pool.len() < POOL_SIZE {
        pool.push(v);
    }
}

/// Single-slot mailbox: keeps the newest frame. If the encoder falls behind,
/// old frames are dropped instead of queueing up (Sunshine's
/// "drop before encode").
#[derive(Default)]
pub struct Mailbox {
    slot: Mutex<Option<CpuFrame>>,
    ready: Condvar,
    pub dropped: std::sync::atomic::AtomicU64,
}

impl Mailbox {
    pub fn put(&self, f: CpuFrame) {
        let old = self.slot.lock().unwrap().replace(f);
        self.ready.notify_one();
        // The frame it replaced is freed (or its buffer recycled) outside the
        // lock, so the video thread never waits on that.
        if old.is_some() {
            self.dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(CpuFrame { data: Pixels::Cpu(buf), .. }) = old {
            recycle(buf);
        }
    }

    pub fn take(&self, timeout: Duration) -> Option<CpuFrame> {
        let slot = self.slot.lock().unwrap();
        let (mut slot, _) = self.ready.wait_timeout_while(slot, timeout, |s| s.is_none()).unwrap();
        slot.take()
    }
}
