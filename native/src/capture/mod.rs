//! Escolhe de onde vem a imagem da tela em cada sistema.

use crate::config::Quality;
use crate::video::Source;

/// TELINHA_FONTE_TESTE=1 troca a tela pela tela de teste (para medir latência);
/// com o caminho de uma imagem, usa ela de fundo (para medir a nitidez).
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
