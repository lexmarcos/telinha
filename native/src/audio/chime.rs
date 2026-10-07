//! Join and leave sounds: two short glassy notes, rising when someone joins
//! the channel and falling when someone leaves. The website (app.js) uses the
//! same formula, so everyone hears the same sound.
//!
//! The streamer's own chime must not end up in the stream (viewers already
//! play theirs): on Linux our playback is left out of the capture, on Windows
//! the capture goes silent while it plays (see `muting`).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::{CHANNELS, RATE};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chime {
    Join,
    Leave,
}

const E5: f32 = 659.25;
const A5: f32 = 880.0;
/// Second note starts this much later.
const STEP: f32 = 0.09;
const NOTE: f32 = 0.5;
const ATTACK: f32 = 0.006;
const DECAY: f32 = 0.11;
const GAIN: f32 = 0.14;

/// Several joins at once (e.g. a whole call clicking Watch) make one sound.
const MIN_GAP_MS: u64 = 250;

fn clock() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Before this moment (in `clock` ms) a new chime is skipped.
static NEXT: AtomicU64 = AtomicU64::new(0);
static MUTE_UNTIL: AtomicU64 = AtomicU64::new(0);

/// Plays the chime in the background (does nothing on systems without audio output).
pub fn play(kind: Chime) {
    let (now, next) = (clock(), NEXT.load(Ordering::Relaxed));
    if now < next || NEXT.compare_exchange(next, now + MIN_GAP_MS, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        return;
    }
    let samples = render(kind);
    let len_ms = (samples.len() / CHANNELS) as u64 * 1000 / RATE as u64;
    // Margin for the output and capture buffers.
    MUTE_UNTIL.store(clock() + len_ms + 200, Ordering::Relaxed);
    #[cfg(target_os = "linux")]
    super::linux::play(samples);
    #[cfg(target_os = "windows")]
    super::windows::play(samples);
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    drop(samples);
}

/// True while our own chime is playing (Windows mutes the capture meanwhile).
pub fn muting() -> bool {
    clock() < MUTE_UNTIL.load(Ordering::Relaxed)
}

/// The chime as interleaved stereo f32 at `RATE`.
pub fn render(kind: Chime) -> Vec<f32> {
    let notes = match kind {
        Chime::Join => [E5, A5],
        Chime::Leave => [A5, E5],
    };
    let frames = ((STEP + NOTE) * RATE as f32) as usize;
    let mut out = Vec::with_capacity(frames * CHANNELS);
    for i in 0..frames {
        let t = i as f32 / RATE as f32;
        let v: f32 = notes.iter().enumerate().map(|(n, &f)| note(f, t - n as f32 * STEP)).sum();
        out.extend(std::iter::repeat_n(v * GAIN, CHANNELS));
    }
    out
}

/// One note: quick attack, exponential decay and a brighter overtone that fades first.
fn note(freq: f32, t: f32) -> f32 {
    if !(0.0..NOTE).contains(&t) {
        return 0.0;
    }
    let env = if t < ATTACK { t / ATTACK } else { (-(t - ATTACK) / DECAY).exp() };
    let tail = ((NOTE - t) / 0.02).min(1.0); // no click at the end
    let w = std::f32::consts::TAU * freq * t;
    env * tail * (w.sin() + 0.2 * (2.0 * w).sin() * (-t / 0.04).exp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chime_is_short_quiet_and_starts_and_ends_silent() {
        for kind in [Chime::Join, Chime::Leave] {
            let s = render(kind);
            assert!(s.len() / CHANNELS < RATE as usize); // under a second
            assert!(s.iter().all(|v| v.abs() < 0.5));
            assert!(s[0].abs() < 1e-3 && s.last().unwrap().abs() < 1e-3);
        }
        assert_ne!(render(Chime::Join), render(Chime::Leave));
    }
}
