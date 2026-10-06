//! Fontes de imagem: PipeWire no Linux (na GPU por DMA-BUF ou na CPU) e a
//! tela de teste.

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

/// Onde a imagem está.
pub enum Pixels {
    /// Na memória, 4 bytes por pixel.
    Cpu(Vec<u8>),
    /// Na GPU (Linux): um buffer DMA-BUF do compositor, sem cópia nenhuma.
    #[cfg(target_os = "linux")]
    DmaBuf { fd: std::os::fd::OwnedFd, offset: u32, size: u32, modifier: u64 },
}

impl Pixels {
    pub fn on_gpu(&self) -> bool {
        !matches!(self, Self::Cpu(_))
    }
}

pub struct CpuFrame {
    /// Imagem de 4 bytes por pixel, na ordem de `pixel`.
    pub data: Pixels,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel: PixelFormat,
    pub captured: Instant,
}

pub trait CpuSource: Send {
    /// Próximo quadro, esperando no máximo `timeout`.
    fn next(&mut self, timeout: Duration) -> Option<CpuFrame>;
}

/// Caixa de correio de um lugar só: guarda o quadro mais novo. Se o
/// codificador atrasar, os quadros antigos são descartados em vez de formar
/// fila (é o "descartar antes de codificar" do Sunshine).
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
