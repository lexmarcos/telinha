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
        let mut slot = self.slot.lock().unwrap();
        if slot.replace(f).is_some() {
            self.dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.ready.notify_one();
    }

    pub fn take(&self, timeout: Duration) -> Option<CpuFrame> {
        let slot = self.slot.lock().unwrap();
        let (mut slot, _) = self.ready.wait_timeout_while(slot, timeout, |s| s.is_none()).unwrap();
        slot.take()
    }
}
