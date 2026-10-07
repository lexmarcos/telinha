//! Native Telinha: the floating bubble that streams the screen with low latency.

// On Windows, no console window behind the bubble.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod audio;
mod capture;
mod config;
mod engine;
mod tray;
mod ui;
mod video;

fn main() {
    init_logs();
    if std::env::args().any(|a| a == "--servidor-padrao") {
        println!("{}", option_env!("TELINHA_SERVER").unwrap_or("(none)"));
        return;
    }

    // Audio test: records N seconds (f32 stereo 48 kHz) to teste-som.raw.
    if let Some(i) = std::env::args().position(|a| a == "--teste-som") {
        let secs: u64 = std::env::args().nth(i + 1).and_then(|s| s.parse().ok()).unwrap_or(5);
        match audio::record(secs) {
            Ok(s) => {
                let bytes: Vec<u8> = s.iter().flat_map(|v| v.to_le_bytes()).collect();
                std::fs::write("teste-som.raw", bytes).expect("file");
                println!("audio: {} samples", s.len());
            }
            Err(e) => println!("audio failed: {e}"),
        }
        return;
    }

    // Chime test: plays join and leave while recording the stream audio, which
    // must stay silent (the chime is only for whoever is at this computer).
    if std::env::args().any(|a| a == "--teste-aviso") {
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            audio::chime::play(audio::chime::Chime::Join);
            std::thread::sleep(std::time::Duration::from_millis(1500));
            audio::chime::play(audio::chime::Chime::Leave);
        });
        match audio::record(5) {
            Ok(s) => println!("stream audio peak while chiming: {:.4}", s.iter().fold(0f32, |m, v| m.max(v.abs()))),
            Err(e) => println!("audio failed: {e}"),
        }
        return;
    }

    // Headless video test: test pattern → encoder → file.
    if let Some(i) = std::env::args().position(|a| a == "--teste-video") {
        let secs: u64 = std::env::args().nth(i + 1).and_then(|s| s.parse().ok()).unwrap_or(5);
        video_test(secs);
        return;
    }

    // Headless: joins a channel by link (or opens one) and streams (testing).
    let args: Vec<String> = std::env::args().collect();
    let secs: u64 = args.iter().skip_while(|a| *a != "--segundos").nth(1).and_then(|s| s.parse().ok()).unwrap_or(20);
    if let Some(i) = args.iter().position(|a| a == "--entrar") {
        let link = args.get(i + 1).cloned().expect("channel link");
        tokio::runtime::Runtime::new().expect("tokio").block_on(headless(Some(link), None, secs));
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--abrir") {
        let server = args.get(i + 1).cloned().expect("server");
        tokio::runtime::Runtime::new().expect("tokio").block_on(headless(None, Some(server), secs));
        return;
    }

    // Wayland doesn't let a window stay always on top or pick its own
    // position; the bubble runs through XWayland (TELINHA_WAYLAND=1 disables).
    // Renders on the CPU: the bubble is small, so it doesn't depend on the GPU
    // driver through XWayland (which doesn't always present via the GPU).
    #[cfg(target_os = "linux")]
    {
        // SAFETY: no other thread exists yet at this point.
        if std::env::var_os("TELINHA_WAYLAND").is_none() {
            unsafe { std::env::remove_var("WAYLAND_DISPLAY") };
        }
        if std::env::var_os("SLINT_BACKEND").is_none() {
            unsafe { std::env::set_var("SLINT_BACKEND", "winit-software") };
        }
    }

    // Engine, Discord and tray on tokio; the UI on the main thread.
    let rt = tokio::runtime::Runtime::new().expect("tokio");
    let _guard = rt.enter();
    app::run();
}

fn video_test(secs: u64) {
    use std::io::Write;
    let quality = config::Quality::default();
    let (tx, mut rx) = tokio::sync::broadcast::channel(64);
    // --captura uses the real screen (system portal); otherwise, the test
    // pattern (TELINHA_FONTE_TESTE=image.png for a static desktop).
    let source = if std::env::args().any(|a| a == "--captura") {
        let rt = tokio::runtime::Runtime::new().expect("tokio");
        match rt.block_on(capture::open(quality)) {
            Ok(s) => {
                std::mem::forget(rt);
                s
            }
            Err(e) => {
                println!("capture failed: {e}");
                return;
            }
        }
    } else if std::env::var_os("TELINHA_FONTE_TESTE").is_none() {
        video::Source::Cpu(Box::new(video::source::test::TestPattern::start(1280, 720, quality.fps)))
    } else {
        let rt = tokio::runtime::Runtime::new().expect("tokio");
        match rt.block_on(capture::open(quality)) {
            Ok(s) => s,
            Err(e) => {
                println!("test source failed: {e}");
                return;
            }
        }
    };
    let bitrate = quality.bitrate((1920, 1080));
    let control = video::start(source, video::Settings { quality, bitrate }, tx);
    let mut file = std::fs::File::create("teste.h264").expect("file");
    let start = std::time::Instant::now();
    let (mut frames, mut keys, mut bytes) = (0u64, 0u64, 0u64);
    let mut asked_key = false;
    while start.elapsed().as_secs() < secs {
        match rx.try_recv() {
            Ok(f) => {
                frames += 1;
                keys += f.key as u64;
                bytes += f.data.len() as u64;
                file.write_all(&f.data).unwrap();
                if frames == 1 {
                    println!("first frame: {} {}x{} keyframe={}", f.codec, f.width, f.height, f.key);
                }
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(2)),
        }
        // Requests a keyframe halfway, like when someone joins.
        if !asked_key && start.elapsed().as_secs() >= secs / 2 {
            control.request_key();
            asked_key = true;
        }
    }
    control.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let st = control.stats();
    let secs_f = start.elapsed().as_secs_f64();
    println!(
        "encoder: {:?}\nframes: {frames} ({:.1} fps), keyframes: {keys}, bitrate: {:.1} Mb/s\nencoding: {:.1} ms per frame, captured {}, error: {:?}",
        st.encoder, frames as f64 / secs_f, bytes as f64 * 8.0 / secs_f / 1e6, st.encode_ms.unwrap_or(0.0), st.captured, st.error
    );
}

async fn headless(link: Option<String>, server: Option<String>, secs: u64) {
    use engine::{Command, Event};
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(engine::session::run(cmd_rx, ev_tx));
    let name = std::env::var("TELINHA_NOME").unwrap_or_else(|_| "App nativo".into());
    let _ = match link {
        Some(input) => cmd_tx.send(Command::Join { input, name, server: None }).await,
        None => cmd_tx.send(Command::Create { name, server }).await,
    };
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        match tokio::time::timeout_at(deadline, ev_rx.recv()).await {
            Ok(Some(e)) => {
                println!("event: {e:?}");
                match e {
                    Event::Joined { .. } => {
                        let discord = std::env::var("TELINHA_DISCORD_SESSAO").ok();
                        let _ = cmd_tx.send(Command::StartLive { quality: config::Quality::default(), discord }).await;
                    }
                    Event::Failed(_) | Event::Left => return,
                    _ => {}
                }
            }
            Ok(None) => return,
            Err(_) => {
                let _ = cmd_tx.send(Command::Leave).await;
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                return;
            }
        }
    }
}

/// Logs to the terminal and to a file (so Windows testers can send it back):
/// Linux ~/.local/share/telinha/telinha.log, Windows %LOCALAPPDATA%\\Telinha\\telinha\\data\\telinha.log.
fn init_logs() {
    use tracing_subscriber::prelude::*;
    let filter = || tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "telinha=info".into());
    let file = directories::ProjectDirs::from("online", "Telinha", "telinha").and_then(|d| {
        std::fs::create_dir_all(d.data_dir()).ok()?;
        std::fs::File::create(d.data_dir().join("telinha.log")).ok()
    });
    let stderr = tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_filter(filter());
    let file = file.map(|f| tracing_subscriber::fmt::layer().with_ansi(false).with_writer(std::sync::Mutex::new(f)).with_filter(filter()));
    tracing_subscriber::registry().with(stderr).with(file).init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), os = std::env::consts::OS, "Telinha started");
}
