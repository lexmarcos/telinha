//! Test pattern: an image that moves all the time (so the encoder has real
//! work to do) with the time stamped in blocks, in the same format as the
//! site's latency tool (tools/latency), to measure end-to-end delay without
//! real capture.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{CpuFrame, CpuSource, Mailbox, PixelFormat, Pixels};

pub struct TestPattern {
    mailbox: Arc<Mailbox>,
    stop: Arc<AtomicBool>,
}

impl TestPattern {
    pub fn start(width: u32, height: u32, fps: u32) -> Self {
        Self::spawn(fps, move |n| draw(width, height, n))
    }

    /// Still image (a real desktop, any size) with a small moving square and
    /// the time in blocks: shows the sharpness that reaches the other side on
    /// a screen that barely changes.
    pub fn over_image(path: &str, fps: u32) -> Result<Self, String> {
        let img = image::open(path).map_err(|e| format!("{path}: {e}"))?.to_rgba8();
        let (w, h) = img.dimensions();
        let mut base = img.into_raw();
        for px in base.chunks_exact_mut(4) {
            px.swap(0, 2); // RGBA → BGRA
        }
        Ok(Self::spawn(fps, move |n| {
            let mut data = base.clone();
            let stride = w * 4;
            // "Cursor" moving slowly, like someone moving the mouse.
            let (cx, cy) = (200 + (n * 4) % w.saturating_sub(400).max(1), h / 3);
            for y in cy..(cy + 24).min(h) {
                for x in cx..(cx + 24).min(w) {
                    let i = (y * stride + x * 4) as usize;
                    data[i..i + 3].copy_from_slice(&[255, 255, 255]);
                }
            }
            stamp(&mut data, w, h, n);
            CpuFrame { data: Pixels::Cpu(data), width: w, height: h, stride, pixel: PixelFormat::Bgrx, captured: Instant::now() }
        }))
    }

    fn spawn(fps: u32, draw: impl Fn(u32) -> CpuFrame + Send + 'static) -> Self {
        let mailbox = Arc::new(Mailbox::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (m, s) = (mailbox.clone(), stop.clone());
        std::thread::Builder::new()
            .name("telinha-teste".into())
            .spawn(move || {
                let period = Duration::from_secs_f64(1.0 / fps as f64);
                let mut next = Instant::now();
                let mut n: u32 = 0;
                while !s.load(Ordering::Relaxed) {
                    m.put(draw(n));
                    n = n.wrapping_add(1);
                    next += period;
                    let now = Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                    } else {
                        next = now;
                    }
                }
            })
            .expect("test pattern thread");
        Self { mailbox, stop }
    }
}

impl Drop for TestPattern {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl CpuSource for TestPattern {
    fn next(&mut self, timeout: Duration) -> Option<CpuFrame> {
        self.mailbox.take(timeout)
    }
}

/// Block grid matching tools/latency/measure.mjs: 16×4 squares of 40 px
/// starting at (48, 48) on a black background with a 16 px border, in a
/// 1280-wide image. Bits: marker 1011, 44 bits of Date.now(), 8 counter bits,
/// CRC-8 (polynomial 0x07) of the middle 52 bits.
pub const CELL: u32 = 40;
const GX: u32 = 48;
const GY: u32 = 48;
const PAD: u32 = 16;

pub fn stamp_bits(ms: u64, counter: u8) -> [bool; 64] {
    let mut bits = [false; 64];
    let mut k = 0;
    let mut push = |v: u64, n: u32, bits: &mut [bool; 64]| {
        for i in (0..n).rev() {
            bits[k] = (v >> i) & 1 == 1;
            k += 1;
        }
    };
    push(0b1011, 4, &mut bits);
    push(ms & ((1 << 44) - 1), 44, &mut bits);
    push(counter as u64, 8, &mut bits);
    let mut crc: u8 = 0;
    for b in &bits[4..56] {
        let top = (crc >> 7) & 1;
        crc <<= 1;
        if (top == 1) != *b {
            crc ^= 0x07;
        }
    }
    push(crc as u64, 8, &mut bits);
    bits
}

fn draw(w: u32, h: u32, n: u32) -> CpuFrame {
    let stride = w * 4;
    let mut data = vec![0u8; (stride * h) as usize];
    let t = n as f32;
    for y in 0..h {
        let row = &mut data[(y * stride) as usize..((y + 1) * stride) as usize];
        for x in 0..w {
            // Moving diagonal stripes + a slowly changing tint.
            let band = ((x + y + n * 6) / 48) % 2;
            let i = (x * 4) as usize;
            row[i] = (90.0 + 60.0 * ((x as f32 / 97.0 + t / 23.0).sin())) as u8;
            row[i + 1] = if band == 0 { 70 } else { 120 };
            row[i + 2] = (110.0 + 80.0 * ((y as f32 / 61.0 - t / 31.0).cos())) as u8;
            row[i + 3] = 255;
        }
    }
    // Block crossing the screen (smoothness is visible from afar).
    let bx = (n * 9) % w.saturating_sub(160).max(1);
    for y in h / 2..(h / 2 + 120).min(h) {
        for x in bx..(bx + 160).min(w) {
            let i = (y * stride + x * 4) as usize;
            data[i..i + 3].copy_from_slice(&[255, 224, 207]);
        }
    }
    stamp(&mut data, w, h, n);
    CpuFrame { data: Pixels::Cpu(data), width: w, height: h, stride, pixel: PixelFormat::Bgrx, captured: Instant::now() }
}

/// Time in blocks (read by the latency tool), scaled to the image width:
/// the grid was designed for a 1280-wide image.
fn stamp(data: &mut [u8], w: u32, h: u32, n: u32) {
    let stride = w * 4;
    let k = w as f32 / 1280.0;
    let at = |v: u32| (v as f32 * k).round() as u32;
    let mut fill = |x0: u32, y0: u32, x1: u32, y1: u32, v: u8| {
        for y in y0..y1.min(h) {
            for x in x0..x1.min(w) {
                let p = (y * stride + x * 4) as usize;
                data[p..p + 3].copy_from_slice(&[v, v, v]);
            }
        }
    };
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let bits = stamp_bits(ms, n as u8);
    fill(at(GX - PAD), at(GY - PAD), at(GX + 16 * CELL + PAD), at(GY + 4 * CELL + PAD), 0);
    for (i, bit) in bits.iter().enumerate() {
        let (cx, cy) = (GX + (i as u32 % 16) * CELL, GY + (i as u32 / 16) * CELL);
        fill(at(cx), at(cy), at(cx + CELL), at(cy + CELL), if *bit { 255 } else { 0 });
    }
}
