//! Picks where the screen image comes from on each OS.

pub mod target;
#[cfg(target_os = "windows")]
pub mod windows;

use tokio::sync::oneshot;

use crate::config::Quality;
use crate::video::Source;
use target::{CaptureKind, CaptureTarget, OpenError, SourceList};

/// A capture that is ready, and what it turned out to be.
pub struct Opened {
    pub source: Source,
    pub kind: CaptureKind,
}

/// TELINHA_FONTE_TESTE=1 replaces the screen with the test pattern (to measure latency);
/// with an image path, uses it as the background (to measure sharpness).
///
/// May take as long as the person takes in the system's picker: run it away
/// from anything that has to keep moving. Sending on `cancel` gives up on it.
pub async fn open(quality: Quality, target: &CaptureTarget, cancel: oneshot::Receiver<()>) -> Result<Opened, OpenError> {
    if let Some(v) = std::env::var_os("TELINHA_FONTE_TESTE") {
        use crate::video::source::test::TestPattern;
        let s = match v.to_str() {
            Some(path) if path != "1" => TestPattern::over_image(path, quality.fps).map_err(OpenError::Failed)?,
            _ => TestPattern::start(1280, 720, quality.fps),
        };
        return Ok(Opened { source: Source::Cpu(Box::new(s)), kind: CaptureKind::Monitor });
    }
    platform(quality, target, cancel).await
}

/// The sources for the app's own picker.
pub fn sources() -> SourceList {
    if std::env::var_os("TELINHA_MOCK").is_some() {
        return crate::engine::mock::sources();
    }
    platform_sources()
}

/// Whether a target picked a moment ago can still be captured, before anything
/// is set up for it. The message is ready to show.
pub fn still_there(target: &CaptureTarget) -> Result<(), String> {
    if std::env::var_os("TELINHA_MOCK").is_some() {
        return Ok(());
    }
    platform_still_there(target)
}

#[cfg(target_os = "windows")]
async fn platform(_: Quality, target: &CaptureTarget, _: oneshot::Receiver<()>) -> Result<Opened, OpenError> {
    // Nothing here waits for the person, but opening a window's capture takes a moment.
    let target = target.clone();
    tokio::task::spawn_blocking(move || windows::open(&target)).await.map_err(|e| OpenError::Failed(e.to_string()))?
}

#[cfg(target_os = "windows")]
fn platform_sources() -> SourceList {
    windows::list()
}

#[cfg(target_os = "windows")]
fn platform_still_there(target: &CaptureTarget) -> Result<(), String> {
    windows::still_there(target)
}

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
async fn platform(quality: Quality, _: &CaptureTarget, cancel: oneshot::Receiver<()>) -> Result<Opened, OpenError> {
    let (capture, kind) = linux::open(quality.fps, cancel).await?;
    Ok(Opened { source: Source::Cpu(Box::new(capture)), kind })
}

/// On Linux the system's portal is the picker.
#[cfg(target_os = "linux")]
fn platform_sources() -> SourceList {
    SourceList::default()
}

#[cfg(target_os = "linux")]
fn platform_still_there(_: &CaptureTarget) -> Result<(), String> {
    Ok(())
}
