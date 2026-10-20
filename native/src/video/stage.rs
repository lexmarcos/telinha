//! The encoder's own thread. When converting (upload, scale, colour) plus
//! encoding take longer than a frame, capture and conversion of one frame
//! overlap the encoding of the previous one: at 4K from memory on an Intel
//! iGPU converting takes ~30 ms and encoding ~20, so the serial loop topped
//! out near 20 fps. Between the two sits a single slot that keeps only the
//! newest converted frame: if the encoder falls behind, the frame it has not
//! started is replaced (and counted as skipped) instead of queueing.
//!
//! Overlapping is not free: both stages then share the GPU and the memory
//! bus, and each gets slower (4K above: ~45 ms converting, ~23 encoding), so
//! a frame takes longer from capture to packet. The capture thread encodes
//! inline, as before, unless overlapping proves worth it (see [`Overlap`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::broadcast;

use super::ffi::{self, Encoder, Frame};
use super::{Codec, Control, EncodedFrame, Timings, h264, h265};

/// A converted frame on its way to the encoder.
pub struct Job {
    pub frame: Frame,
    pub captured: Instant,
    /// When conversion finished (the encode time counts from here).
    pub converted: Instant,
    pub size: (u32, u32),
    pub codec: Codec,
}

struct Shared {
    slot: Mutex<Option<Job>>,
    ready: Condvar,
    /// None while the pipeline is being rebuilt.
    encoder: Mutex<Option<Encoder>>,
    /// What turns packets into frames (locked after `encoder`).
    out: Mutex<Output>,
    stop: AtomicBool,
    failed: Mutex<Option<ffi::Error>>,
}

struct Output {
    headers: h264::Headers,
    hevc: h265::Headers,
    /// pts -> (capture time, size), for packets that come out after their frame.
    pending: HashMap<i64, (Instant, (u32, u32))>,
    index: u64,
    /// Average encode time (ms), for [`Overlap`].
    encode_ms: f32,
}

pub struct EncodeStage {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl EncodeStage {
    pub fn start(control: Arc<Control>, out: broadcast::Sender<Arc<EncodedFrame>>) -> Self {
        let shared = Arc::new(Shared {
            slot: Mutex::new(None),
            ready: Condvar::new(),
            encoder: Mutex::new(None),
            out: Mutex::new(Output { headers: h264::Headers::default(), hevc: h265::Headers::default(), pending: HashMap::new(), index: 0, encode_ms: 0.0 }),
            stop: AtomicBool::new(false),
            failed: Mutex::new(None),
        });
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name("telinha-encode".into())
            .spawn(move || {
                let _boost = crate::realtime::boost(crate::realtime::Task::Video);
                if let Err(e) = encode_loop(&s, &control, &out) {
                    *s.failed.lock().unwrap() = Some(e);
                }
            })
            .expect("encode thread");
        Self { shared, thread: Some(thread) }
    }

    /// Encodes on the calling thread, waiting for the encoder thread to
    /// finish what it has (so frames stay in order).
    pub fn encode_here(&self, job: Job, control: &Control, out: &broadcast::Sender<Arc<EncodedFrame>>) -> ffi::Result<()> {
        let mut guard = self.shared.encoder.lock().unwrap();
        let Some(encoder) = guard.as_mut() else { return Ok(()) };
        // A frame handed over before switching to inline goes first.
        let queued = self.shared.slot.lock().unwrap().take();
        for job in queued.into_iter().chain([job]) {
            encode(encoder, &self.shared.out, job, control, out)?;
        }
        Ok(())
    }

    /// Average encode time (ms).
    pub fn encode_ms(&self) -> f32 {
        self.shared.out.lock().unwrap().encode_ms
    }

    /// Hands over the newest converted frame. Returns whether it replaced
    /// one the encoder had not started (a skipped frame).
    pub fn submit(&self, job: Job) -> bool {
        let old = self.shared.slot.lock().unwrap().replace(job);
        self.shared.ready.notify_one();
        // Freed outside the lock.
        old.is_some()
    }

    /// The encoder, waiting for the frame being encoded to finish. Holding the
    /// guard pauses encoding (to change the bitrate or rebuild).
    pub fn encoder(&self) -> MutexGuard<'_, Option<Encoder>> {
        self.shared.encoder.lock().unwrap()
    }

    /// Takes the encoder out for a rebuild; frames converted for it are
    /// dropped (they belong to the old pipeline).
    pub fn remove(&self) -> Option<Encoder> {
        let mut enc = self.encoder();
        drop(self.shared.slot.lock().unwrap().take());
        enc.take()
    }

    pub fn install(&self, encoder: Encoder) {
        *self.encoder() = Some(encoder);
    }

    /// The encoder thread stopped on an error.
    pub fn failure(&self) -> Option<ffi::Error> {
        self.shared.failed.lock().unwrap().take()
    }
}

impl Drop for EncodeStage {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.ready.notify_one();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // The encoder goes before the graph and devices the caller drops next.
        drop(self.remove());
    }
}

fn encode_loop(shared: &Shared, control: &Control, out: &broadcast::Sender<Arc<EncodedFrame>>) -> ffi::Result<()> {
    loop {
        {
            let slot = shared.slot.lock().unwrap();
            let _ = shared.ready.wait_timeout_while(slot, Duration::from_millis(100), |s| s.is_none() && !shared.stop.load(Ordering::Relaxed)).unwrap();
        }
        if shared.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        // The job is taken with the encoder held, as the inline path does:
        // whichever takes a frame encodes it before anyone takes the next.
        let mut guard = shared.encoder.lock().unwrap();
        let Some(encoder) = guard.as_mut() else { continue };
        let Some(job) = shared.slot.lock().unwrap().take() else { continue };
        encode(encoder, &shared.out, job, control, out)?;
    }
}

fn encode(encoder: &mut Encoder, output: &Mutex<Output>, job: Job, control: &Control, out: &broadcast::Sender<Arc<EncodedFrame>>) -> ffi::Result<()> {
    let Job { mut frame, captured, converted, size, codec } = job;
    let mut o = output.lock().unwrap();
    let key = control.force_key.swap(false, Ordering::Relaxed);
    frame.set_keyframe(key);
    o.pending.insert(frame.pts(), (captured, size));
    let started = Instant::now();
    encoder.send(&mut frame)?;
    drop(frame);
    while let Some(pkt) = encoder.receive()? {
        let (captured, size) = o.pending.remove(&pkt.pts).unwrap_or((converted, size));
        let ms = captured.elapsed().as_secs_f32() * 1000.0;
        let took = started.elapsed().as_secs_f32() * 1000.0;
        o.encode_ms = if o.encode_ms == 0.0 { took } else { o.encode_ms * 0.9 + took * 0.1 };
        {
            let mut t = control.timings.lock().unwrap();
            Timings::add(&mut t.encode, converted.elapsed());
            Timings::add(&mut t.total, captured.elapsed());
        }
        {
            let mut st = control.stats.lock().unwrap();
            st.encoded += 1;
            st.encode_ms = Some(st.encode_ms.map_or(ms, |p| p * 0.9 + ms * 0.1));
        }
        let (data, codec) = match codec {
            Codec::H264 => {
                let data = o.headers.fix_keyframe(pkt.data, pkt.key);
                (data, o.headers.codec())
            }
            Codec::Hevc => {
                let data = o.hevc.fix_keyframe(pkt.data, pkt.key);
                (data, o.hevc.codec())
            }
        };
        let Some(codec) = codec else { continue };
        o.index += 1;
        let frame = Arc::new(EncodedFrame { data: Bytes::from(data), key: pkt.key, codec, width: size.0, height: size.1, captured, index: o.index });
        // Into the cache before it goes out: a sender subscribed to `out`
        // and then reading the cache sees every frame at least once.
        control.gop.lock().unwrap().push(&frame);
        let _ = out.send(frame);
    }
    if o.pending.len() > 240 {
        o.pending.clear();
    }
    Ok(())
}

/// Whether the capture thread hands frames to the encoder thread (overlap)
/// or encodes them itself. Inline by default. Once converting plus encoding
/// take longer than a frame period for ten frames in a row, overlapping gets
/// a two-second trial and stays only if it encodes at least 15% more frames
/// per second than inline did just before; otherwise it is not tried again
/// for 30 s. (On an Intel iGPU at 4K from memory it won 4% more frames for
/// 20 ms more delay per frame: the stages share the GPU and memory bus.)
/// It goes back inline when the two fit well within a period again.
/// TELINHA_SOBREPOR=0/1 forces either (for measuring).
pub struct Overlap {
    state: State,
    forced: Option<bool>,
    streak: u32,
    /// Since when (and from which encoded count) every frame ran late, inline.
    behind: Option<(Instant, u64)>,
    retry_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Inline,
    Trial { since: Instant, encoded: u64, inline_rate: f32 },
    On,
}

const WINDOW: Duration = Duration::from_secs(2);
const TRIAL_GAIN: f32 = 1.15;
const RETRY: Duration = Duration::from_secs(30);

impl Overlap {
    pub fn new() -> Self {
        let forced = std::env::var("TELINHA_SOBREPOR").ok().map(|v| v == "1");
        Self { state: State::Inline, forced, streak: 0, behind: None, retry_at: None }
    }

    /// One frame's conversion time and the encoder's average (ms), the frame
    /// period (ms) and how many frames were encoded so far. Returns whether
    /// to overlap.
    pub fn update(&mut self, now: Instant, convert_ms: f32, encode_ms: f32, period_ms: f32, encoded: u64) -> bool {
        if let Some(f) = self.forced {
            return f;
        }
        let busy = convert_ms + encode_ms;
        match self.state {
            State::Inline => {
                // Late for a whole window, inline: that window's rate is the
                // one a trial has to beat.
                if busy <= period_ms || self.retry_at.is_some_and(|t| now < t) {
                    self.behind = None;
                } else {
                    let (since, start) = *self.behind.get_or_insert((now, encoded));
                    let took = now.saturating_duration_since(since);
                    if took >= WINDOW {
                        let inline_rate = (encoded - start.min(encoded)) as f32 / took.as_secs_f32();
                        self.behind = None;
                        self.state = State::Trial { since: now, encoded, inline_rate };
                    }
                }
            }
            State::Trial { since, encoded: start, inline_rate } => {
                let took = now.saturating_duration_since(since);
                if took >= WINDOW {
                    let rate = (encoded - start.min(encoded)) as f32 / took.as_secs_f32();
                    let keep = rate >= inline_rate * TRIAL_GAIN;
                    tracing::info!(keep, inline_fps = inline_rate, overlapped_fps = rate, "encoder thread trial");
                    self.state = if keep { State::On } else { State::Inline };
                    if !keep {
                        self.retry_at = Some(now + RETRY);
                    }
                }
            }
            State::On => {
                self.streak = if busy < period_ms * 0.7 { self.streak + 1 } else { 0 };
                if self.streak >= 10 {
                    self.streak = 0;
                    self.state = State::Inline;
                }
            }
        }
        self.state != State::Inline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds frames at `fps` encoded for `secs`, returning the last verdict.
    fn run(o: &mut Overlap, t: &mut Instant, n: &mut u64, fps: f32, busy: f32, secs: f32) -> bool {
        let mut last = false;
        for _ in 0..(fps * secs) as u32 {
            *t += Duration::from_secs_f32(1.0 / fps);
            *n += 1;
            last = o.update(*t, busy / 2.0, busy / 2.0, 16.7, *n);
        }
        last
    }

    #[test]
    fn sobrepoe_so_se_render_mais_quadros() {
        let mut t = Instant::now();
        let mut o = Overlap { state: State::Inline, forced: None, streak: 0, behind: None, retry_at: None };
        let mut n = 0;
        // 1080p60: fits in the period, stays inline.
        assert!(!run(&mut o, &mut t, &mut n, 60.0, 10.0, 5.0));
        // 4K from memory: two seconds behind at 20 fps, then a trial...
        assert!(!run(&mut o, &mut t, &mut n, 20.0, 50.0, 1.9));
        assert!(run(&mut o, &mut t, &mut n, 20.0, 50.0, 0.2));
        // ...where overlapping gives 20.6 fps: not worth it, and not tried for 30 s.
        assert!(!run(&mut o, &mut t, &mut n, 20.6, 50.0, 2.0));
        assert!(!run(&mut o, &mut t, &mut n, 20.0, 50.0, 29.0));
        // Then it tries again, and now overlapping gives 30 fps: kept.
        assert!(run(&mut o, &mut t, &mut n, 20.0, 50.0, 3.5));
        assert!(run(&mut o, &mut t, &mut n, 30.0, 50.0, 2.1));
        assert!(run(&mut o, &mut t, &mut n, 30.0, 50.0, 5.0));
        // Well within the period again: inline.
        assert!(!run(&mut o, &mut t, &mut n, 60.0, 8.0, 1.0));
    }
}
