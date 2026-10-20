//! Minimum-latency mode protocol, identical to the site's turbo.js:
//!
//!   u8 0x54 | u8 flags (1 = keyframe, 2 = has config) | u16 index | u16 total
//!   u16 config length | u32 sequence | f64 timestamp (µs) | [config JSON] | data
//!
//! Parity (flags 4): index = total + group, data = u32 frame size | XOR of the
//! group's fragments zero-padded to the longest (fragment i is in group
//! i % groups, groups = ceil(total / FEC_GROUP)). Viewers from before it drop
//! it (index >= total); it only goes to viewers that report `fec`, while they
//! are losing data.
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
const FEC_GROUP: usize = 5;
/// Parity stays on this long after the last loss a viewer reported.
pub const FEC_HOLD: Duration = Duration::from_secs(30);

fn fec_groups(count: usize) -> usize {
    if count < 2 { 0 } else { count.div_ceil(FEC_GROUP) }
}

/// Splits a frame into packets. The config (codec and size) goes in the first
/// packet of keyframes, so viewers joining midway can configure the decoder.
/// With `parity`, a parity packet per group of fragments follows.
pub fn packets(seq: u32, frame: &EncodedFrame, ts_us: f64, parity: bool) -> Vec<BytesMut> {
    let config = frame.key.then(|| format!(r#"{{"codec":"{}","w":{},"h":{}}}"#, frame.codec, frame.width, frame.height));
    let data = &frame.data;
    let count = data.len().div_ceil(FRAG).max(1);
    let pack = |flags: u8, idx: usize, extra: &[u8], body: &[u8]| {
        let mut b = BytesMut::with_capacity(HEADER + extra.len() + body.len());
        b.put_u8(MAGIC);
        b.put_u8(flags | u8::from(frame.key) | if extra.is_empty() { 0 } else { 2 });
        b.put_u16_le(idx as u16);
        b.put_u16_le(count as u16);
        b.put_u16_le(extra.len() as u16);
        b.put_u32_le(seq);
        b.put_f64_le(ts_us);
        b.put_slice(extra);
        b.put_slice(body);
        b
    };
    let config = config.as_deref().unwrap_or("").as_bytes();
    let fragment = |i: usize| &data[i * FRAG..((i + 1) * FRAG).min(data.len())];
    let mut out: Vec<BytesMut> = (0..count).map(|i| pack(0, i, if i == 0 { config } else { &[] }, fragment(i))).collect();
    let groups = if parity { fec_groups(count) } else { 0 };
    for g in 0..groups {
        let mut x = vec![0u8; 4 + FRAG.min(data.len())];
        x[..4].copy_from_slice(&(data.len() as u32).to_le_bytes());
        for i in (g..count).step_by(groups) {
            for (d, s) in x[4..].iter_mut().zip(fragment(i)) {
                *d ^= s;
            }
        }
        out.push(pack(4, count + g, config, &x));
    }
    out
}

/// A sender's counters (read by the session for reports and the panel).
#[derive(Default)]
pub struct Counters {
    pub frames: AtomicU64,
    pub bytes: AtomicU64,
    /// Frames this viewer did not get: queue full, or waiting for a keyframe.
    pub dropped: AtomicU64,
}

/// Rate-limits one viewer's keyframe requests. Each viewer has its own, so
/// one asking does not hold back another who just joined; requests that
/// land in the same frame still make a single keyframe (one encoder).
///
/// A viewer that keeps asking backs off: 500 ms, then 1 s between keyframes
/// (never longer: the picture stays frozen until one comes), back to the
/// start after a few calm seconds. A keyframe is many frames' worth of bytes
/// for everyone; on a link that keeps losing, asking as fast as allowed only
/// makes the loss worse.
pub struct KeyLimiter {
    state: std::sync::Mutex<KeyState>,
}

#[derive(Default)]
struct KeyState {
    last: Option<Instant>,
    backoff: Duration,
}

const KEY_BACKOFF_START: Duration = Duration::from_millis(500);
const KEY_BACKOFF_MAX: Duration = Duration::from_secs(1);
/// No request for this long: the backoff starts over.
const KEY_CALM: Duration = Duration::from_secs(4);

impl KeyLimiter {
    pub fn new() -> Self {
        Self { state: std::sync::Mutex::new(KeyState::default()) }
    }

    /// Asks the encoder for a keyframe unless one was asked within `min` (or
    /// the backoff, if longer). Returns whether it asked.
    pub fn request(&self, control: &Control, min: Duration) -> bool {
        let asked = self.allow(Instant::now(), min);
        if asked {
            control.request_key();
        }
        asked
    }

    fn allow(&self, now: Instant, min: Duration) -> bool {
        let mut st = self.state.lock().unwrap();
        if st.last.is_none_or(|t| now.saturating_duration_since(t) >= KEY_CALM) {
            st.backoff = Duration::ZERO;
        }
        if st.last.is_some_and(|t| now.saturating_duration_since(t) < min.max(st.backoff)) {
            return false;
        }
        st.last = Some(now);
        st.backoff = (st.backoff * 2).clamp(KEY_BACKOFF_START, KEY_BACKOFF_MAX);
        true
    }
}

pub struct Sender {
    pub counters: Arc<Counters>,
    /// This viewer's keyframe requests (its sender's and its own messages).
    pub keys: Arc<KeyLimiter>,
    /// Parity packets go out until then (the viewer reported losses).
    pub fec_until: Arc<std::sync::Mutex<Option<Instant>>>,
    stop: Arc<AtomicBool>,
}

/// How many average frames may wait in the channel's send queue. Past that
/// the frame is dropped (and a keyframe follows once it drains) instead of
/// adding delay: the old 300 ms budget let latency grow that much before
/// anything gave way.
const QUEUE_FRAMES: f64 = 3.0;

/// Bytes allowed in the send queue: about three average frames (~50 ms),
/// plus what is left of the last keyframe still draining at the target
/// bitrate (a keyframe is many frames' worth: without that allowance, the
/// frames after it would be dropped and another keyframe asked for).
fn budget(bitrate: u32, fps: u32, key: Option<(usize, Instant)>, now: Instant) -> usize {
    let bytes_per_s = bitrate as f64 / 8.0;
    let frames = (bytes_per_s / fps.max(1) as f64 * QUEUE_FRAMES) as usize;
    let key_left = key.map_or(0.0, |(size, at)| size as f64 - now.saturating_duration_since(at).as_secs_f64() * bytes_per_s);
    frames.max(32 * 1024) + key_left.max(0.0) as usize
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
    bitrate: Arc<AtomicU32>,
    fps: u32,
    epoch: Instant,
) -> Sender {
    let counters = Arc::new(Counters::default());
    let keys = Arc::new(KeyLimiter::new());
    let stop = Arc::new(AtomicBool::new(false));
    let fec_until = Arc::new(std::sync::Mutex::new(None::<Instant>));
    let (c, s, k, fec) = (counters.clone(), stop.clone(), keys.clone(), fec_until.clone());
    tokio::spawn(async move {
        let keys = k;
        let mut seq: u32 = 0;
        let mut need_key = true;
        // Size and send time of the last keyframe, for the queue budget.
        let mut last_key: Option<(usize, Instant)> = None;
        // Start from the cached keyframe when what came after it fits in
        // about 100 ms of the bitrate (a still screen, or a keyframe someone
        // else just asked for); otherwise ask for a new one.
        let allowance = bitrate.load(Ordering::Relaxed) as usize / 8 / 10;
        let mut replayed: u64 = 0;
        match control.replay(|_, rest| rest <= allowance) {
            Some(gop) => {
                for frame in &gop {
                    let ts = frame.captured.saturating_duration_since(epoch).as_micros() as f64;
                    if !send_frame(&channel, seq, frame, ts, false).await {
                        return;
                    }
                    seq = seq.wrapping_add(1);
                    c.frames.fetch_add(1, Ordering::Relaxed);
                    c.bytes.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
                }
                need_key = false;
                last_key = gop.first().map(|k| (k.data.len(), Instant::now()));
                replayed = gop.last().map_or(0, |f| f.index);
                tracing::debug!(frames = gop.len(), "new viewer starts from the cached keyframe");
            }
            None => {
                keys.request(&control, Duration::ZERO);
            }
        }
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
            if frame.index <= replayed {
                continue; // already sent from the cache
            }
            let budget = budget(bitrate.load(Ordering::Relaxed), fps, last_key, Instant::now());
            let queued = channel.outstanding_bytes().await.unwrap_or(0);
            if need_key && !frame.key {
                // Not delivered either: the viewer's picture is frozen meanwhile.
                c.dropped.fetch_add(1, Ordering::Relaxed);
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
            let parity = fec.lock().unwrap().is_some_and(|t| Instant::now() < t);
            if !send_frame(&channel, seq, &frame, ts, parity).await {
                break;
            }
            seq = seq.wrapping_add(1);
            if frame.key {
                need_key = false;
                last_key = Some((frame.data.len(), Instant::now()));
            }
            c.frames.fetch_add(1, Ordering::Relaxed);
            c.bytes.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
        }
    });
    Sender { counters, keys, fec_until, stop }
}

async fn send_frame(channel: &Arc<dyn DataChannel>, seq: u32, frame: &EncodedFrame, ts: f64, parity: bool) -> bool {
    for p in packets(seq, frame, ts, parity) {
        if channel.send(p).await.is_err() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pedidos_de_keyframe_recuam() {
        let k = KeyLimiter::new();
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let min = Duration::from_millis(300);
        assert!(k.allow(at(0), min));
        // Next ones wait 500 ms, then 1 s at most.
        assert!(!k.allow(at(400), min));
        assert!(k.allow(at(500), min));
        assert!(!k.allow(at(1400), min));
        assert!(k.allow(at(1500), min));
        assert!(!k.allow(at(2400), min));
        assert!(k.allow(at(2500), min));
        assert!(k.allow(at(3500), min));
        // Calm for a while: back to the short wait.
        assert!(k.allow(at(10_000), min));
        assert!(k.allow(at(10_500), min));
        // A longer minimum from the caller still holds.
        assert!(!k.allow(at(11_000), Duration::from_secs(1)));
    }

    #[test]
    fn splits_like_the_site() {
        let f = EncodedFrame {
            data: bytes::Bytes::from(vec![7u8; 40_000]),
            key: true,
            codec: "avc1.640028".into(),
            width: 1920,
            height: 1080,
            captured: Instant::now(),
            index: 1,
        };
        let ps = packets(9, &f, 1234.0, false);
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

    #[test]
    fn paridade_refaz_um_pedaco_perdido() {
        let data: Vec<u8> = (0..70_000u32).map(|i| (i * 7 + i / 300) as u8).collect();
        let f = EncodedFrame { data: bytes::Bytes::from(data.clone()), key: true, codec: "avc1.640028".into(), width: 1920, height: 1080, captured: Instant::now(), index: 1 };
        let ps = packets(3, &f, 0.0, true);
        // 5 fragments, one parity group.
        assert_eq!(ps.len(), 6);
        let p = &ps[5];
        assert_eq!(p[1], 1 | 2 | 4);
        assert_eq!(u16::from_le_bytes([p[2], p[3]]), 5);
        let cfg_len = u16::from_le_bytes([p[6], p[7]]) as usize;
        let body = &p[HEADER + cfg_len..];
        assert_eq!(u32::from_le_bytes(body[..4].try_into().unwrap()), 70_000);
        // Lose fragment 4 (the short last one): XOR of parity and the rest.
        let mut x = body[4..].to_vec();
        for i in 0..4 {
            for (d, s) in x.iter_mut().zip(&data[i * FRAG..(i + 1) * FRAG]) {
                *d ^= s;
            }
        }
        assert_eq!(&x[..70_000 - 4 * FRAG], &data[4 * FRAG..]);
        // Single-fragment frames get none; 12 fragments get 3 interleaved groups.
        let small = EncodedFrame { data: bytes::Bytes::from(vec![1u8; 100]), ..f };
        assert_eq!(packets(4, &small, 0.0, true).len(), 1);
        assert_eq!(fec_groups(12), 3);
        if let Some(path) = std::env::var_os("TURBO_DUMP") {
            // For the cross-check against turbo.js (see tools/latency).
            let all: Vec<u8> = ps.iter().flat_map(|p| (p.len() as u32).to_le_bytes().into_iter().chain(p.iter().copied())).collect();
            std::fs::write(path, all).unwrap();
        }
    }

    #[test]
    fn fila_de_poucos_quadros_mais_a_keyframe() {
        let now = Instant::now();
        // 40 Mb/s at 60 fps: 3 frames of ~83 kB.
        assert_eq!(budget(40_000_000, 60, None, now), 250_000);
        // Low bitrates keep a floor.
        assert_eq!(budget(2_000_000, 60, None, now), 32 * 1024);
        // A 400 kB keyframe just sent counts on top, draining at the bitrate.
        assert_eq!(budget(40_000_000, 60, Some((400_000, now)), now), 650_000);
        let later = now + Duration::from_millis(40);
        assert_eq!(budget(40_000_000, 60, Some((400_000, now)), later), 450_000);
        assert_eq!(budget(40_000_000, 60, Some((400_000, now)), now + Duration::from_secs(1)), 250_000);
    }
}
