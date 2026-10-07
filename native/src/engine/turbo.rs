//! Minimum-latency mode protocol, identical to the site's turbo.js:
//!
//!   u8 0x54 | u8 flags (1 = keyframe, 2 = has config) | u16 index | u16 total
//!   u16 config length | u32 sequence | f64 timestamp (µs) | [config JSON] | data
//!
//! Each viewer has its own sender, which receives the already encoded frames
//! (encoded once, for everyone) and manages that connection's queue.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::{BufMut, BytesMut};
use tokio::sync::broadcast;
use webrtc::data_channel::DataChannel;

use crate::video::{Control, EncodedFrame};

const MAGIC: u8 = 0x54;
const HEADER: usize = 20;
const FRAG: usize = 16000;

/// Splits a frame into packets. The config (codec and size) goes in the first
/// packet of keyframes, so viewers joining midway can configure the decoder.
pub fn packets(seq: u32, frame: &EncodedFrame, ts_us: f64) -> Vec<BytesMut> {
    let config = frame.key.then(|| format!(r#"{{"codec":"{}","w":{},"h":{}}}"#, frame.codec, frame.width, frame.height));
    let count = frame.data.len().div_ceil(FRAG).max(1);
    (0..count)
        .map(|i| {
            let body = &frame.data[i * FRAG..((i + 1) * FRAG).min(frame.data.len())];
            let extra = if i == 0 { config.as_deref().unwrap_or("").as_bytes() } else { &[] };
            let mut b = BytesMut::with_capacity(HEADER + extra.len() + body.len());
            b.put_u8(MAGIC);
            b.put_u8(u8::from(frame.key) | if extra.is_empty() { 0 } else { 2 });
            b.put_u16_le(i as u16);
            b.put_u16_le(count as u16);
            b.put_u16_le(extra.len() as u16);
            b.put_u32_le(seq);
            b.put_f64_le(ts_us);
            b.put_slice(extra);
            b.put_slice(body);
            b
        })
        .collect()
}

/// A sender's counters (read by the session for reports and the panel).
#[derive(Default)]
pub struct Counters {
    pub frames: AtomicU64,
    pub bytes: AtomicU64,
    pub dropped: AtomicU64,
}

/// Rate-limits keyframe requests coming from several places at once.
pub struct KeyLimiter {
    last: std::sync::Mutex<Option<Instant>>,
}

impl KeyLimiter {
    pub fn new() -> Self {
        Self { last: std::sync::Mutex::new(None) }
    }

    pub fn request(&self, control: &Control, min: Duration) {
        let mut last = self.last.lock().unwrap();
        if last.is_none_or(|t| t.elapsed() >= min) {
            *last = Some(Instant::now());
            control.request_key();
        }
    }
}

pub struct Sender {
    pub counters: Arc<Counters>,
    stop: Arc<AtomicBool>,
}

impl Sender {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn spawn(
    channel: Arc<dyn DataChannel>,
    mut frames: broadcast::Receiver<Arc<EncodedFrame>>,
    control: Arc<Control>,
    keys: Arc<KeyLimiter>,
    bitrate: Arc<AtomicU32>,
    epoch: Instant,
) -> Sender {
    let counters = Arc::new(Counters::default());
    let stop = Arc::new(AtomicBool::new(false));
    let (c, s) = (counters.clone(), stop.clone());
    tokio::spawn(async move {
        let mut seq: u32 = 0;
        let mut need_key = true;
        keys.request(&control, Duration::ZERO);
        while !s.load(Ordering::Relaxed) {
            let frame = match tokio::time::timeout(Duration::from_millis(500), frames.recv()).await {
                Ok(Ok(f)) => f,
                Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                    // Fell behind: frames were lost, a keyframe is needed.
                    c.dropped.fetch_add(n, Ordering::Relaxed);
                    need_key = true;
                    continue;
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Err(_) => continue,
            };
            let budget = (bitrate.load(Ordering::Relaxed) as usize / 8 * 3 / 10).max(256 * 1024); // ~300 ms
            let queued = channel.outstanding_bytes().await.unwrap_or(0);
            if need_key && !frame.key {
                // Only request the keyframe once the queue drained, otherwise it won't fit either.
                if queued < budget / 2 {
                    keys.request(&control, Duration::from_secs(1));
                }
                continue;
            }
            if queued > budget {
                need_key = true;
                c.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let ts = frame.captured.saturating_duration_since(epoch).as_micros() as f64;
            let mut failed = false;
            for p in packets(seq, &frame, ts) {
                if channel.send(p).await.is_err() {
                    failed = true;
                    break;
                }
            }
            if failed {
                break;
            }
            seq = seq.wrapping_add(1);
            if frame.key {
                need_key = false;
            }
            c.frames.fetch_add(1, Ordering::Relaxed);
            c.bytes.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
        }
    });
    Sender { counters, stop }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_the_site() {
        let f = EncodedFrame {
            data: bytes::Bytes::from(vec![7u8; 40_000]),
            key: true,
            codec: "avc1.640028".into(),
            width: 1920,
            height: 1080,
            captured: Instant::now(),
        };
        let ps = packets(9, &f, 1234.0);
        assert_eq!(ps.len(), 3);
        let p0 = &ps[0];
        assert_eq!(p0[0], MAGIC);
        assert_eq!(p0[1], 3); // keyframe + config
        assert_eq!(u16::from_le_bytes([p0[4], p0[5]]), 3);
        let cfg_len = u16::from_le_bytes([p0[6], p0[7]]) as usize;
        let cfg = std::str::from_utf8(&p0[HEADER..HEADER + cfg_len]).unwrap();
        assert_eq!(cfg, r#"{"codec":"avc1.640028","w":1920,"h":1080}"#);
        assert_eq!(u32::from_le_bytes([p0[8], p0[9], p0[10], p0[11]]), 9);
        assert_eq!(ps[1][1], 1); // keyframe without config
        let total: usize = ps.iter().enumerate().map(|(i, p)| p.len() - HEADER - if i == 0 { cfg_len } else { 0 }).sum();
        assert_eq!(total, 40_000);
    }
}
