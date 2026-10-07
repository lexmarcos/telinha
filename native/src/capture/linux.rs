//! Linux screen capture through the system portal (the same GNOME/KDE screen
//! picker, works on Wayland and X11) and PipeWire.
//!
//! The first time, the system asks which screen; the portal returns a token
//! and later captures start directly.
//!
//! The image preferably comes as DMA-BUF: the compositor hands over a buffer
//! that is already on the GPU and the encoder imports it without copying. A 4K
//! screen through memory is 33 MB per frame uploaded back to the card; on Iris
//! Xe that alone caps everything at ~25 fps. If the compositor refuses, it
//! falls back to memory.

use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use pipewire as pw;
use pw::spa;

use crate::video::source::{CpuFrame, CpuSource, Mailbox, PixelFormat, Pixels};

fn token_path() -> Option<std::path::PathBuf> {
    directories::ProjectDirs::from("online", "Telinha", "telinha").map(|d| d.config_dir().join("captura.token"))
}

fn load_token() -> Option<String> {
    token_path().and_then(|p| std::fs::read_to_string(p).ok()).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

fn save_token(t: &str) {
    if let Some(p) = token_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, t);
    }
}

pub struct PortalCapture {
    mailbox: Arc<Mailbox>,
    stop: Arc<AtomicBool>,
    // The portal session must live while capture runs, and be closed at the
    // end: without Close, the system's "sharing screen" indicator stays on
    // until the app exits.
    portal: Option<(Screencast, ashpd::desktop::Session<Screencast>)>,
    /// Capture teardown happens on the video thread, outside tokio.
    rt: tokio::runtime::Handle,
}

impl CpuSource for PortalCapture {
    fn next(&mut self, timeout: Duration) -> Option<CpuFrame> {
        self.mailbox.take(timeout)
    }
}

impl Drop for PortalCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some((proxy, session)) = self.portal.take() {
            self.rt.spawn(async move {
                if let Err(e) = session.close().await {
                    tracing::warn!("close capture session: {e}");
                }
                drop(proxy);
            });
        }
    }
}

pub async fn open(fps: u32) -> Result<PortalCapture, String> {
    let fail = |e: ashpd::Error| match e {
        ashpd::Error::Response(_) => "A captura foi cancelada no seletor de tela.".to_owned(),
        e => format!("O portal de captura do sistema falhou: {e}"),
    };
    let proxy = Screencast::new().await.map_err(fail)?;
    let session = proxy.create_session(Default::default()).await.map_err(fail)?;
    let token = load_token();
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(SourceType::Monitor | SourceType::Window)
                .set_multiple(false)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token.as_deref()),
        )
        .await
        .map_err(fail)?;
    let response = proxy.start(&session, None, Default::default()).await.map_err(fail)?.response().map_err(fail)?;
    if let Some(t) = response.restore_token() {
        save_token(t);
    }
    let node = response.streams().first().map(|s| s.pipe_wire_node_id()).ok_or("O portal não devolveu nenhuma tela.")?;
    let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await.map_err(fail)?;

    let mailbox = Arc::new(Mailbox::default());
    let stop = Arc::new(AtomicBool::new(false));
    let (m, s) = (mailbox.clone(), stop.clone());
    std::thread::Builder::new()
        .name("telinha-pipewire".into())
        .spawn(move || {
            if let Err(e) = run_pipewire(fd, node, fps, m, s) {
                tracing::error!("PipeWire: {e}");
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(PortalCapture { mailbox, stop, portal: Some((proxy, session)), rt: tokio::runtime::Handle::current() })
}

struct Format {
    pixel: Option<PixelFormat>,
    width: u32,
    height: u32,
    /// DRM modifier when the image comes as DMA-BUF.
    modifier: Option<u64>,
}

/// DRM_FORMAT_MOD_LINEAR: pixels in rows, no tiling. Every VAAPI driver
/// imports it and it can be read on the CPU if needed.
const MOD_LINEAR: i64 = 0;

fn run_pipewire(fd: OwnedFd, node: u32, fps: u32, mailbox: Arc<Mailbox>, stop: Arc<AtomicBool>) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
    let core = context.connect_fd_rc(fd, None).map_err(|e| e.to_string())?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "telinha",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| e.to_string())?;

    let _listener = stream
        .add_local_listener_with_user_data(Format { pixel: None, width: 0, height: 0, modifier: None })
        .param_changed(|stream, fmt, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = spa::param::video::VideoInfoRaw::default();
            if info.parse(param).is_err() {
                return;
            }
            fmt.width = info.size().width;
            fmt.height = info.size().height;
            fmt.pixel = match info.format() {
                spa::param::video::VideoFormat::BGRx | spa::param::video::VideoFormat::BGRA => Some(PixelFormat::Bgrx),
                spa::param::video::VideoFormat::RGBx | spa::param::video::VideoFormat::RGBA => Some(PixelFormat::Rgbx),
                _ => None,
            };
            let dmabuf = info.flags().bits() & spa::sys::SPA_VIDEO_FLAG_MODIFIER != 0;
            fmt.modifier = dmabuf.then(|| info.modifier());
            tracing::info!(w = fmt.width, h = fmt.height, format = ?info.format(), dmabuf, "capture negotiated");
            // Says which buffer type we accept: DMA-BUF if the format came with
            // a modifier, memory otherwise.
            let types = if dmabuf { 1 << spa::buffer::DataType::DmaBuf.as_raw() } else { (1 << spa::buffer::DataType::MemPtr.as_raw()) | (1 << spa::buffer::DataType::MemFd.as_raw()) };
            let buffers = spa::pod::Value::Object(spa::pod::Object {
                type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
                id: spa::param::ParamType::Buffers.as_raw(),
                properties: vec![spa::pod::Property::new(
                    spa::sys::SPA_PARAM_BUFFERS_dataType,
                    spa::pod::Value::Choice(spa::pod::ChoiceValue::Int(spa::utils::Choice(
                        spa::utils::ChoiceFlags::empty(),
                        spa::utils::ChoiceEnum::Flags { default: types as i32, flags: vec![] },
                    ))),
                )],
            });
            if let Ok(bytes) = serialize(&buffers) {
                if let Some(pod) = spa::pod::Pod::from_bytes(&bytes) {
                    let _ = stream.update_params(&mut [pod]);
                }
            }
        })
        .process(move |stream, fmt| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Some(pixel) = fmt.pixel else { return };
            let datas = buffer.datas_mut();
            let Some(d) = datas.first_mut() else { return };
            let chunk = d.chunk();
            let (offset, size, stride) = (chunk.offset() as usize, chunk.size() as usize, chunk.stride());
            if size == 0 || stride <= 0 {
                return; // cursor-only or unchanged frame
            }
            let data = if d.type_() == spa::buffer::DataType::DmaBuf {
                // The fd belongs to PipeWire; a dup of it lives while the encoder uses it.
                let Some(modifier) = fmt.modifier else { return };
                let Ok(fd) = unsafe { std::os::fd::BorrowedFd::borrow_raw(d.fd() as i32) }.try_clone_to_owned() else { return };
                Pixels::DmaBuf { fd, offset: offset as u32, size: d.as_raw().maxsize, modifier }
            } else {
                let Some(bytes) = d.data() else { return };
                let Some(src) = bytes.get(offset..offset + size) else { return };
                Pixels::Cpu(src.to_vec())
            };
            mailbox.put(CpuFrame { data, width: fmt.width, height: fmt.height, stride: stride as u32, pixel, captured: Instant::now() });
        })
        .register()
        .map_err(|e| e.to_string())?;

    // First the format with a modifier (DMA-BUF); then the same without (memory).
    // TELINHA_SEM_DMABUF=1 forces memory.
    let with_gpu = std::env::var_os("TELINHA_SEM_DMABUF").is_none();
    let mut pods: Vec<Vec<u8>> = Vec::new();
    if with_gpu {
        let mut obj = video_format(fps);
        obj.properties.push(spa::pod::Property {
            key: spa::param::format::FormatProperties::VideoModifier.as_raw(),
            flags: spa::pod::PropertyFlags::from_bits_retain(spa::sys::SPA_POD_PROP_FLAG_MANDATORY | spa::sys::SPA_POD_PROP_FLAG_DONT_FIXATE),
            value: spa::pod::Value::Choice(spa::pod::ChoiceValue::Long(spa::utils::Choice(
                spa::utils::ChoiceFlags::empty(),
                spa::utils::ChoiceEnum::Enum { default: MOD_LINEAR, alternatives: vec![MOD_LINEAR] },
            ))),
        });
        pods.push(serialize(&spa::pod::Value::Object(obj))?);
    }
    pods.push(serialize(&spa::pod::Value::Object(video_format(fps)))?);
    let mut params: Vec<&spa::pod::Pod> = pods.iter().map(|b| spa::pod::Pod::from_bytes(b).ok_or("invalid format")).collect::<Result<_, _>>()?;
    stream
        .connect(spa::utils::Direction::Input, Some(node), pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS, &mut params)
        .map_err(|e| e.to_string())?;

    // Stops when the capture is dropped (end of the stream).
    let ml = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop.load(Ordering::Relaxed) {
            ml.quit();
        }
    });
    timer.update_timer(Some(Duration::from_millis(100)), Some(Duration::from_millis(100))).into_result().map_err(|e| e.to_string())?;
    mainloop.run();
    Ok(())
}

fn serialize(v: &spa::pod::Value) -> Result<Vec<u8>, String> {
    Ok(spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), v).map_err(|e| format!("{e:?}"))?.0.into_inner())
}

/// Raw BGRX/RGBX video, any size, up to `fps` frames per second.
fn video_format(fps: u32) -> spa::pod::Object {
    spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(spa::param::format::FormatProperties::MediaType, Id, spa::param::format::MediaType::Video),
        spa::pod::property!(spa::param::format::FormatProperties::MediaSubtype, Id, spa::param::format::MediaSubtype::Raw),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRA,
            spa::param::video::VideoFormat::RGBx,
            spa::param::video::VideoFormat::RGBA,
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle { width: 1920, height: 1080 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: 8192, height: 8192 }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: fps, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 240, denom: 1 }
        ),
    )
}
