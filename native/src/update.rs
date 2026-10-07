//! Updates from the server the app was built for.
//!
//! Only builds made with both TELINHA_SERVER and TELINHA_UPDATE_KEY update
//! themselves (public builds do not). The server publishes in /download:
//!   latest.json      version and, per package, its file, size and SHA-256
//!   latest.json.sig  Ed25519 signature of latest.json (base64)
//! The signature is checked with the public key baked in at build time, so
//! whoever controls the web server alone cannot push a fake update.
//!
//! How each package updates itself:
//!   AppImage  the new file replaces the old one in place
//!   .deb      installed with pkexec (the system asks for the password)
//!   Windows   the files are swapped in the app's folder: Windows allows
//!             renaming files in use, so the old ones become *.old and are
//!             deleted on the next start (installed and portable alike)

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::Value;

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: String,
    /// Absolute address of this system's package and its checks.
    url: String,
    sha256: String,
    size: u64,
}

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    AppImage(PathBuf),
    Deb,
    Windows(PathBuf),
}

/// How this copy was installed (None: development build or unknown).
fn kind() -> Option<Kind> {
    #[cfg(target_os = "linux")]
    {
        if let Some(p) = std::env::var_os("APPIMAGE") {
            return Some(Kind::AppImage(p.into()));
        }
        if std::env::current_exe().ok()? == Path::new("/usr/bin/telinha") {
            return Some(Kind::Deb);
        }
        None
    }
    #[cfg(target_os = "windows")]
    {
        Some(Kind::Windows(std::env::current_exe().ok()?.parent()?.to_path_buf()))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    None
}

/// Where the updates come from; None when this build does not update itself.
fn source() -> Option<(String, Vec<u8>)> {
    let key = B64.decode(option_env!("TELINHA_UPDATE_KEY")?).ok().filter(|k| k.len() == 32)?;
    // TELINHA_UPDATE_URL is only for tests (a local folder served over HTTP).
    let base = match std::env::var("TELINHA_UPDATE_URL") {
        Ok(url) => url,
        Err(_) => format!("https://{}/download", crate::config::default_server()?),
    };
    Some((base.trim_end_matches('/').to_owned(), key))
}

/// The download page, for when the app cannot update itself.
pub fn page() -> Option<String> {
    crate::config::default_server().map(|s| format!("https://{s}/baixar.html"))
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder().timeout(Duration::from_secs(30)).build().map_err(|e| e.to_string())
}

/// Asks the server for the latest version. Ok(None): up to date, or this
/// build does not update itself.
pub async fn check() -> Result<Option<Release>, String> {
    let Some((base, key)) = source() else { return Ok(None) };
    let client = client()?;
    let get = |path: String| {
        let client = client.clone();
        async move {
            let r = client.get(path).send().await.map_err(|e| e.to_string())?;
            if !r.status().is_success() {
                return Err(format!("HTTP {}", r.status()));
            }
            r.bytes().await.map_err(|e| e.to_string())
        }
    };
    let manifest = get(format!("{base}/latest.json")).await?;
    let sig = get(format!("{base}/latest.json.sig")).await?;
    let sig = B64.decode(String::from_utf8_lossy(&sig).trim()).map_err(|_| "assinatura inválida")?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &key)
        .verify(&manifest, &sig)
        .map_err(|_| "a assinatura da atualização não confere")?;
    let v: Value = serde_json::from_slice(&manifest).map_err(|e| e.to_string())?;
    let version = v["version"].as_str().ok_or("manifesto sem versão")?.to_owned();
    if !newer(&version, CURRENT) {
        return Ok(None);
    }
    let package = match kind() {
        Some(Kind::AppImage(_)) => "appimage",
        Some(Kind::Deb) => "deb",
        Some(Kind::Windows(_)) => "windows",
        None => "",
    };
    let f = &v["files"][package];
    Ok(Some(Release {
        version,
        url: f["file"].as_str().map(|p| format!("{base}/{p}")).unwrap_or_default(),
        sha256: f["sha256"].as_str().unwrap_or_default().to_lowercase(),
        size: f["size"].as_u64().unwrap_or(0),
    }))
}

impl Release {
    /// A made-up release, for the screenshot script.
    pub fn sample(version: &str) -> Self {
        Self { version: version.into(), url: String::new(), sha256: String::new(), size: 0 }
    }
}

/// Can this copy install the release by itself (or only send the person to the page)?
pub fn can_install(r: &Release) -> bool {
    kind().is_some() && !r.url.is_empty() && r.sha256.len() == 64
}

/// "1.10.0" is newer than "1.9.3".
pub fn newer(a: &str, b: &str) -> bool {
    let parse = |s: &str| s.split('.').map(|n| n.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    parse(a) > parse(b)
}

/// Downloads, checks and installs the release, then starts the new version.
/// The caller quits right after. `progress` goes from 0 to 1.
pub async fn install(r: Release, progress: impl Fn(f32)) -> Result<(), String> {
    let kind = kind().ok_or("esta cópia não se atualiza sozinha")?;
    let file = match &kind {
        // Next to the AppImage, so the final rename stays on the same disk.
        Kind::AppImage(p) => p.with_file_name(".telinha-atualizacao"),
        _ => std::env::temp_dir().join(format!("telinha-{}{}", r.version, if kind == Kind::Deb { ".deb" } else { ".zip" })),
    };
    download(&r, &file, progress).await?;
    let started = tokio::task::spawn_blocking(move || apply(&kind, &file)).await.map_err(|e| e.to_string())?;
    started
}

async fn download(r: &Release, file: &Path, progress: impl Fn(f32)) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(20)).build().map_err(|e| e.to_string())?;
    let mut resp = client.get(&r.url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("download: HTTP {}", resp.status()));
    }
    let total = resp.content_length().unwrap_or(r.size).max(1);
    let mut out = tokio::fs::File::create(file).await.map_err(|e| format!("{}: {e}", file.display()))?;
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut got = 0u64;
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        hash.update(&chunk);
        out.write_all(&chunk).await.map_err(|e| e.to_string())?;
        got += chunk.len() as u64;
        progress(got as f32 / total as f32);
    }
    out.flush().await.map_err(|e| e.to_string())?;
    let digest: String = hash.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect();
    if digest != r.sha256 {
        let _ = tokio::fs::remove_file(file).await;
        return Err("o arquivo baixado veio corrompido".into());
    }
    Ok(())
}

fn apply(kind: &Kind, file: &Path) -> Result<(), String> {
    match kind {
        Kind::AppImage(target) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
            }
            std::fs::rename(file, target).map_err(|e| format!("trocar o AppImage: {e}"))?;
            restart(target)
        }
        Kind::Deb => {
            let ok = std::process::Command::new("pkexec")
                .args(["dpkg", "-i"])
                .arg(file)
                .status()
                .map_err(|e| format!("pkexec: {e}"))?
                .success();
            let _ = std::fs::remove_file(file);
            if !ok {
                return Err("a instalação foi cancelada".into());
            }
            restart(Path::new("/usr/bin/telinha"))
        }
        Kind::Windows(dir) => {
            swap_from_zip(file, dir)?;
            let _ = std::fs::remove_file(file);
            restart(&dir.join("telinha.exe"))
        }
    }
}

/// Unpacks the zip next to the app and swaps each file in.
fn swap_from_zip(zip: &Path, dir: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let staging = dir.join(".atualizacao");
        let _ = std::fs::remove_dir_all(&staging);
        let f = std::fs::File::open(zip).map_err(|e| e.to_string())?;
        zip::ZipArchive::new(f).and_then(|mut a| a.extract(&staging)).map_err(|e| format!("abrir a atualização: {e}"))?;
        for entry in std::fs::read_dir(&staging).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if !entry.path().is_file() {
                continue;
            }
            let target = dir.join(entry.file_name());
            if target.exists() {
                // A file in use cannot be replaced, but it can be renamed.
                let mut old = target.with_extension(format!("{}.old", target.extension().and_then(|e| e.to_str()).unwrap_or("")));
                if std::fs::remove_file(&old).is_err() && old.exists() {
                    old = old.with_extension(format!("{}.old", std::process::id()));
                }
                std::fs::rename(&target, &old).map_err(|e| format!("{}: {e}", target.display()))?;
            }
            std::fs::rename(entry.path(), &target).map_err(|e| format!("{}: {e}", target.display()))?;
        }
        let _ = std::fs::remove_dir_all(&staging);
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (zip, dir);
        Err("pacote do Windows".into())
    }
}

/// Starts the new version; it shows that it was updated.
fn restart(exe: &Path) -> Result<(), String> {
    use std::process::Stdio;
    let mut cmd = std::process::Command::new(exe);
    // Detached from this copy's terminal (it logs to its own file anyway).
    cmd.env("TELINHA_ATUALIZADO", CURRENT).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // The AppImage runtime sets these for the old copy; the new one sets its own.
    for k in ["APPIMAGE", "APPDIR", "ARGV0", "OWD"] {
        cmd.env_remove(k);
    }
    cmd.spawn().map(|_| ()).map_err(|e| format!("abrir a nova versão: {e}"))
}

/// Leftovers of the previous update (Windows renames the files in use).
pub fn cleanup() {
    if let Some(Kind::Windows(dir)) = kind()
        && let Ok(entries) = std::fs::read_dir(&dir)
    {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().ends_with(".old") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::newer;

    #[test]
    fn compares_versions_by_number() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("1.10.0", "1.9.3"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
    }
}
