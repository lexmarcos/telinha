//! Telinha nativo: a bolha flutuante que transmite a tela com baixa latência.

// No Windows, sem janela de console atrás da bolha.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod audio;
mod capture;
mod config;
mod discord;
mod engine;
mod tray;
mod ui;
mod video;

fn main() {
    init_logs();
    if std::env::args().any(|a| a == "--servidor-padrao") {
        println!("{}", option_env!("TELINHA_SERVER").unwrap_or("(nenhum)"));
        return;
    }

    // Teste do som: grava N segundos (f32 estéreo 48 kHz) em teste-som.raw.
    if let Some(i) = std::env::args().position(|a| a == "--teste-som") {
        let secs: u64 = std::env::args().nth(i + 1).and_then(|s| s.parse().ok()).unwrap_or(5);
        match audio::record(secs) {
            Ok(s) => {
                let bytes: Vec<u8> = s.iter().flat_map(|v| v.to_le_bytes()).collect();
                std::fs::write("teste-som.raw", bytes).expect("arquivo");
                println!("som: {} amostras", s.len());
            }
            Err(e) => println!("som falhou: {e}"),
        }
        return;
    }

    // Teste do vídeo sem interface: tela de teste → codificador → arquivo.
    if let Some(i) = std::env::args().position(|a| a == "--teste-video") {
        let secs: u64 = std::env::args().nth(i + 1).and_then(|s| s.parse().ok()).unwrap_or(5);
        video_test(secs);
        return;
    }

    // Sem interface: entra num canal pelo link (ou abre um) e transmite (testes).
    let args: Vec<String> = std::env::args().collect();
    let secs: u64 = args.iter().skip_while(|a| *a != "--segundos").nth(1).and_then(|s| s.parse().ok()).unwrap_or(20);
    if let Some(i) = args.iter().position(|a| a == "--entrar") {
        let link = args.get(i + 1).cloned().expect("link do canal");
        tokio::runtime::Runtime::new().expect("tokio").block_on(headless(Some(link), None, secs));
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--abrir") {
        let server = args.get(i + 1).cloned().expect("servidor");
        tokio::runtime::Runtime::new().expect("tokio").block_on(headless(None, Some(server), secs));
        return;
    }

    // O Wayland não deixa uma janela ficar sempre no topo nem escolher a
    // própria posição; a bolha roda pelo XWayland (TELINHA_WAYLAND=1 desliga).
    // Desenha na CPU: a bolha é pequena e assim não depende do driver de vídeo
    // pelo XWayland (que nem sempre apresenta pela GPU).
    #[cfg(target_os = "linux")]
    {
        // SAFETY: ainda não existe nenhuma outra thread neste ponto.
        if std::env::var_os("TELINHA_WAYLAND").is_none() {
            unsafe { std::env::remove_var("WAYLAND_DISPLAY") };
        }
        if std::env::var_os("SLINT_BACKEND").is_none() {
            unsafe { std::env::set_var("SLINT_BACKEND", "winit-software") };
        }
    }

    // Motor, Discord e bandeja no tokio; a interface na thread principal.
    let rt = tokio::runtime::Runtime::new().expect("tokio");
    let _guard = rt.enter();
    app::run();
}

fn video_test(secs: u64) {
    use std::io::Write;
    let quality = config::Quality::default();
    let (tx, mut rx) = tokio::sync::broadcast::channel(64);
    // --captura usa a tela de verdade (portal do sistema); sem isso, a tela de
    // teste (TELINHA_FONTE_TESTE=imagem.png para uma área de trabalho parada).
    let source = if std::env::args().any(|a| a == "--captura") {
        let rt = tokio::runtime::Runtime::new().expect("tokio");
        match rt.block_on(capture::open(quality)) {
            Ok(s) => {
                std::mem::forget(rt);
                s
            }
            Err(e) => {
                println!("captura falhou: {e}");
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
                println!("fonte de teste falhou: {e}");
                return;
            }
        }
    };
    let bitrate = quality.bitrate((1920, 1080));
    let control = video::start(source, video::Settings { quality, bitrate }, tx);
    let mut file = std::fs::File::create("teste.h264").expect("arquivo");
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
                    println!("primeiro quadro: {} {}x{} keyframe={}", f.codec, f.width, f.height, f.key);
                }
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(2)),
        }
        // Pede uma keyframe no meio, como quando alguém entra.
        if !asked_key && start.elapsed().as_secs() >= secs / 2 {
            control.request_key();
            asked_key = true;
        }
    }
    control.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let st = control.stats();
    let secs_f = start.elapsed().as_secs_f64();
    println!(
        "codificador: {:?}\nquadros: {frames} ({:.1} fps), keyframes: {keys}, taxa: {:.1} Mb/s\ncodificação: {:.1} ms por quadro, capturados {}, erro: {:?}",
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
    // Rich Presence também aqui (para testar sem a interface).
    let (presence, rx) = tokio::sync::watch::channel(None::<discord::Presence>);
    tokio::spawn(discord::run(rx));
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        match tokio::time::timeout_at(deadline, ev_rx.recv()).await {
            Ok(Some(e)) => {
                println!("evento: {e:?}");
                match e {
                    Event::Joined { invite, .. } => {
                        presence.send_replace(Some(discord::Presence { invite, live: false, viewers: 0, since }));
                        let discord = std::env::var("TELINHA_DISCORD_SESSAO").ok();
                        let _ = cmd_tx.send(Command::StartLive { quality: config::Quality::default(), discord }).await;
                    }
                    Event::Live(live) => presence.send_modify(|p| {
                        if let Some(p) = p {
                            p.live = live;
                        }
                    }),
                    Event::Viewers(v) => presence.send_modify(|p| {
                        if let Some(p) = p {
                            p.viewers = v.len();
                        }
                    }),
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

/// Logs no terminal e num arquivo (para quem testa no Windows mandar de volta):
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
    tracing::info!(versao = env!("CARGO_PKG_VERSION"), so = std::env::consts::OS, "Telinha começou");
}
