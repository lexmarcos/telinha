//! Picks where the screen image comes from on each OS.

use crate::config::Quality;
use crate::video::Source;

/// TELINHA_FONTE_TESTE=1 replaces the screen with the test pattern (to measure latency);
/// with an image path, uses it as the background (to measure sharpness).
pub async fn open(quality: Quality) -> Result<Source, String> {
    if let Some(v) = std::env::var_os("TELINHA_FONTE_TESTE") {
        use crate::video::source::test::TestPattern;
        let s = match v.to_str() {
            Some(path) if path != "1" => TestPattern::over_image(path, quality.fps)?,
            _ => TestPattern::start(1280, 720, quality.fps),
        };
        return Ok(Source::Cpu(Box::new(s)));
    }
    platform(quality).await
}

#[cfg(target_os = "windows")]
async fn platform(_: Quality) -> Result<Source, String> {
    Ok(Source::Desktop)
}

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
async fn platform(quality: Quality) -> Result<Source, String> {
    Ok(Source::Cpu(Box::new(linux::open(quality.fps).await?)))
}
