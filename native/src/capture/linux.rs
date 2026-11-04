//! Linux screen capture through the system portal (the same GNOME/KDE screen
//! picker, works on Wayland and X11) and PipeWire.
//!
//! The system asks what to share (a monitor or a window) on every stream: nothing
//! is remembered between them, since "the same as last time" is how someone ends
//! up showing the wrong thing. The portal's answer says only which kind was
//! picked and which PipeWire node carries it: no process, so a window's sound
//! cannot be worked out from here (see `audio::plan`).
//!
//! A window does not fill its buffer. GNOME hands over a buffer as big as the
//! monitor and says where the window is in it, on every frame (`SPA_META_VideoCrop`):
//! the window can be resized without renegotiating anything, and without that
//! information the viewers get a small window in a sea of black.
//!
//! Nothing else says when a stream ends. The node disappears when the window
//! closes or the person stops sharing from the system's menu, and the PipeWire
//! stream then just sits idle: that is watched here, and reported through
//! `CpuSource::ended`.
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
use tokio::sync::oneshot;

use super::target::{CaptureKind, OpenError, WindowSound};
use crate::video::source::{CpuFrame, CpuSource, Mailbox, PixelFormat, Pixels};

/// Older versions kept a restore token here, which skipped the picker forever.
fn forget_old_token() {
    if let Some(d) = crate::config::dirs() {
        let _ = std::fs::remove_file(d.config_dir().join("captura.token"));
    }
}

pub struct PortalCapture {
    mailbox: Arc<Mailbox>,
    stop: Arc<AtomicBool>,
    /// The source went away, and how to say so.
    ended: Arc<AtomicBool>,
    ended_note: &'static str,
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

    fn ended(&self) -> Option<String> {
        self.ended.load(Ordering::Relaxed).then(|| self.ended_note.to_owned())
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

const WINDOW_ENDED: &str = "A janela que você estava transmitindo foi fechada, ou o sistema parou o compartilhamento. A transmissão parou.";
const SCREEN_ENDED: &str = "A tela que você estava transmitindo saiu do ar, ou o sistema parou o compartilhamento. A transmissão parou.";

/// A portal that does not say what was picked counts as a window: the worst
/// that does is ask which app to hear, instead of sending the whole computer's sound.
fn kind_of(source: Option<SourceType>) -> CaptureKind {
    match source {
        Some(SourceType::Monitor | SourceType::Virtual) => CaptureKind::Monitor,
        Some(SourceType::Window) | None => CaptureKind::Window { title: None, sound: WindowSound::AskApp },
    }
}

/// Starts reading a node. `fd`: the portal's restricted connection to PipeWire.
fn read_node(fd: Option<OwnedFd>, node: u32, fps: u32, kind: &CaptureKind, portal: Option<(Screencast, ashpd::desktop::Session<Screencast>)>) -> Result<PortalCapture, OpenError> {
    let mailbox = Arc::new(Mailbox::default());
    let stop = Arc::new(AtomicBool::new(false));
    let ended = Arc::new(AtomicBool::new(false));
    let (m, s, e) = (mailbox.clone(), stop.clone(), ended.clone());
    std::thread::Builder::new()
        .name("telinha-pipewire".into())
        .spawn(move || {
            if let Err(err) = run_pipewire(fd, node, fps, m, s, e.clone()) {
                tracing::error!("PipeWire: {err}");
                e.store(true, Ordering::Relaxed);
            }
        })
        .map_err(|e| OpenError::Failed(e.to_string()))?;
    let ended_note = if kind.is_window() { WINDOW_ENDED } else { SCREEN_ENDED };
    Ok(PortalCapture { mailbox, stop, ended, ended_note, portal, rt: tokio::runtime::Handle::current() })
}

pub async fn open(fps: u32, cancel: oneshot::Receiver<()>) -> Result<(PortalCapture, CaptureKind), OpenError> {
    // Testing only: reads a node on the default PipeWire with no portal, because
    // the system's picker needs a person. TELINHA_CAPTURA_TIPO=monitor for a monitor.
    if let Some(node) = std::env::var("TELINHA_CAPTURA_NO").ok().and_then(|n| n.parse().ok()) {
        let kind = kind_of((std::env::var("TELINHA_CAPTURA_TIPO").as_deref() == Ok("monitor")).then_some(SourceType::Monitor));
        return Ok((read_node(None, node, fps, &kind, None)?, kind));
    }
    forget_old_token();
    let fail = |e: ashpd::Error| match e {
        ashpd::Error::Response(_) => OpenError::Cancelled,
        e => OpenError::Failed(format!("O portal de captura do sistema falhou: {e}")),
    };
    let proxy = Screencast::new().await.map_err(fail)?;
    let session = proxy.create_session(Default::default()).await.map_err(fail)?;
    let picked = async {
        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(CursorMode::Embedded)
                    .set_sources(SourceType::Monitor | SourceType::Window)
                    .set_multiple(false)
                    .set_persist_mode(PersistMode::DoNot),
            )
            .await
            .map_err(fail)?;
        proxy.start(&session, None, Default::default()).await.map_err(fail)?.response().map_err(fail)
    };
    // The picker waits on the person; the engine may want to give up on it first.
    // A dropped sender is not a request to cancel.
    let cancelled = async {
        if cancel.await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    let response = tokio::select! {
        r = picked => r?,
        () = cancelled => {
            let _ = session.close().await;
            return Err(OpenError::Cancelled);
        }
    };
    let stream = response.streams().first().ok_or(OpenError::Failed("O portal não devolveu nenhuma tela.".into()))?;
    let (node, kind) = (stream.pipe_wire_node_id(), kind_of(stream.source_type()));
    tracing::info!(node, ?kind, "the portal picked a source");
    let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await.map_err(fail)?;
    Ok((read_node(Some(fd), node, fps, &kind, Some((proxy, session)))?, kind))
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

fn run_pipewire(fd: Option<OwnedFd>, node: u32, fps: u32, mailbox: Arc<Mailbox>, stop: Arc<AtomicBool>, ended: Arc<AtomicBool>) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
    let core = match fd {
        Some(fd) => context.connect_fd_rc(fd, None),
        None => context.connect_rc(None),
    }
    .map_err(|e| e.to_string())?;
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

    // When the node goes away the stream only goes idle, without an error. The
    // registry says so outright; a stream in error, or dropped by the server, too.
    let (gone_node, gone_stream) = (ended.clone(), ended.clone());
    let registry = core.get_registry_rc().map_err(|e| e.to_string())?;
    let _registry_listener = registry
        .add_listener_local()
        .global_remove(move |id| {
            if id == node {
                tracing::info!(node, "the captured node is gone");
                gone_node.store(true, Ordering::Relaxed);
            }
        })
        .register();

    let _listener = stream
        .add_local_listener_with_user_data(Format { pixel: None, width: 0, height: 0, modifier: None })
        .state_changed(move |_, _, old, new| {
            tracing::debug!(?old, ?new, "capture stream state");
            if capture_over(&old, &new) {
                tracing::info!(?old, ?new, "the capture stream ended");
                gone_stream.store(true, Ordering::Relaxed);
            }
        })
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
            let max = info.max_framerate();
            tracing::info!(w = fmt.width, h = fmt.height, format = ?info.format(), dmabuf, max_fps = format!("{}/{}", max.num, max.denom), "capture negotiated");
            // Says which buffer type we accept: DMA-BUF if the format came with
            // a modifier, memory otherwise.
            let types = if dmabuf { 1 << spa::buffer::DataType::DmaBuf.as_raw() } else { (1 << spa::buffer::DataType::MemPtr.as_raw()) | (1 << spa::buffer::DataType::MemFd.as_raw()) };
            // Where the picture is in each buffer (see the top of this file).
            let crop = spa::pod::Value::Object(spa::pod::Object {
                type_: spa::utils::SpaTypes::ObjectParamMeta.as_raw(),
                id: spa::param::ParamType::Meta.as_raw(),
                properties: vec![
                    spa::pod::Property::new(spa::sys::SPA_PARAM_META_type, spa::pod::Value::Id(spa::utils::Id(spa::sys::SPA_META_VideoCrop))),
                    spa::pod::Property::new(spa::sys::SPA_PARAM_META_size, spa::pod::Value::Int(std::mem::size_of::<spa::sys::spa_meta_region>() as i32)),
                ],
            });
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
            if let (Ok(a), Ok(b)) = (serialize(&buffers), serialize(&crop)) {
                if let (Some(a), Some(b)) = (spa::pod::Pod::from_bytes(&a), spa::pod::Pod::from_bytes(&b)) {
                    let _ = stream.update_params(&mut [a, b]);
                }
            }
        })
        .process(move |stream, fmt| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Some(pixel) = fmt.pixel else { return };
            let crop = buffer.find_meta::<spa::buffer::meta::MetaVideoCrop>().map(|m| m.meta_region()).filter(|r| r.is_valid()).map(|r| (r.position().x, r.position().y, r.size().width, r.size().height));
            let view = visible_part(crop, (fmt.width, fmt.height));
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
                Pixels::DmaBuf { fd, offset: offset as u32, size: d.as_raw().maxsize, modifier, buffer: (fmt.width, fmt.height), corner: (view.x, view.y) }
            } else {
                // Only the rows with the picture, starting at its first pixel; the
                // stride stays the buffer's.
                let start = offset + view.y as usize * stride as usize + view.x as usize * 4;
                let len = (view.height as usize).saturating_sub(1) * stride as usize + view.width as usize * 4;
                let Some(bytes) = d.data() else { return };
                let Some(src) = bytes.get(start..start + len).filter(|_| start + len <= offset + size) else { return };
                let mut buf = crate::video::source::buffer(len);
                buf.extend_from_slice(src);
                Pixels::Cpu(buf)
            };
            mailbox.put(CpuFrame { data, width: view.width, height: view.height, stride: stride as u32, pixel, captured: Instant::now() });
        })
        .register()
        .map_err(|e| e.to_string())?;

    // First the format with a modifier (DMA-BUF); then the same without (memory).
    // TELINHA_SEM_DMABUF=1 forces memory.
    let with_gpu = std::env::var_os("TELINHA_SEM_DMABUF").is_none() && !nvidia();
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

/// The part of a buffer with the picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct View {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

/// Where a crop says the picture is, within what the buffer has. All of it when
/// there is no crop, or none that makes sense.
fn visible_part(crop: Option<(i32, i32, u32, u32)>, (width, height): (u32, u32)) -> View {
    let whole = View { x: 0, y: 0, width, height };
    let Some((x, y, w, h)) = crop else { return whole };
    let (Ok(x), Ok(y)) = (u32::try_from(x), u32::try_from(y)) else { return whole };
    if x >= width || y >= height || w == 0 || h == 0 {
        return whole;
    }
    View { x, y, width: w.min(width - x), height: h.min(height - y) }
}

/// An error at any point ends the capture. So does going back to unconnected,
/// which only happens without us asking when the server dropped the stream: on
/// the way up it starts unconnected, so it has to have left that state first.
fn capture_over(old: &pw::stream::StreamState, new: &pw::stream::StreamState) -> bool {
    use pw::stream::StreamState::{Error, Unconnected};
    matches!(new, Error(_)) || (matches!(new, Unconnected) && !matches!(old, Unconnected))
}

/// Whether the GPU that VAAPI would use is NVIDIA's. Its proprietary driver
/// has no VAAPI encoder, so a DMA-BUF frame only went back to memory
/// (hwdownload: reading GPU memory through a mapping, slow, and failing on
/// some driver versions) for NVENC to upload it again. Asking for memory from
/// the start skips that round trip: NVENC takes BGRx straight from it.
/// (Importing the DMA-BUF into CUDA through EGL would skip the copy as well;
/// not done yet.)
fn nvidia() -> bool {
    let node = crate::video::render_node();
    let name = std::path::Path::new(&node).file_name().and_then(|n| n.to_str()).unwrap_or("renderD128").to_owned();
    let vendor = std::fs::read_to_string(format!("/sys/class/drm/{name}/device/vendor")).unwrap_or_default();
    let nvidia = vendor.trim().eq_ignore_ascii_case("0x10de");
    if nvidia {
        tracing::info!(node, "NVIDIA GPU: capturing to memory for NVENC");
    }
    nvidia
}

fn serialize(v: &spa::pod::Value) -> Result<Vec<u8>, String> {
    Ok(spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), v).map_err(|e| format!("{e:?}"))?.0.into_inner())
}

/// Raw BGRX/RGBX video, any size. `fps` is only a hint for compositors that
/// capture on a timer (wlroots); GNOME sends on every refresh where the screen
/// changed, and the video thread holds the stream to the chosen rate (Pacer).
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
        // The compositor's own frame limit: as high as it allows. Left out,
        // it is whatever the compositor proposes, which could be lower than
        // the screen. GNOME caps it at the refresh rate anyway (and then
        // skips frames that come a hair early: one more reason not to ask
        // for less).
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 240, denom: 1 },
            spa::utils::Fraction { num: 1, denom: 1 },
            spa::utils::Fraction { num: 240, denom: 1 }
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pw::stream::StreamState::{Connecting, Error, Paused, Streaming, Unconnected};

    #[test]
    fn portal_que_nao_diz_o_tipo_conta_como_janela() {
        assert_eq!(kind_of(Some(SourceType::Monitor)), CaptureKind::Monitor);
        assert_eq!(kind_of(Some(SourceType::Virtual)), CaptureKind::Monitor);
        let janela = CaptureKind::Window { title: None, sound: WindowSound::AskApp };
        assert_eq!(kind_of(Some(SourceType::Window)), janela);
        assert_eq!(kind_of(None), janela);
    }

    #[test]
    fn so_a_parte_com_a_imagem() {
        let view = |x, y, width, height| View { x, y, width, height };
        // No crop: the whole buffer.
        assert_eq!(visible_part(None, (1280, 800)), view(0, 0, 1280, 800));
        // A window inside a monitor-sized buffer, as GNOME sends it.
        assert_eq!(visible_part(Some((35, 28, 800, 500)), (1280, 800)), view(35, 28, 800, 500));
        // Past the edge: only what the buffer has.
        assert_eq!(visible_part(Some((1000, 700, 800, 500)), (1280, 800)), view(1000, 700, 280, 100));
        // Nonsense: the whole buffer, rather than nothing.
        assert_eq!(visible_part(Some((-1, 0, 800, 500)), (1280, 800)), view(0, 0, 1280, 800));
        assert_eq!(visible_part(Some((2000, 0, 800, 500)), (1280, 800)), view(0, 0, 1280, 800));
        assert_eq!(visible_part(Some((0, 0, 0, 500)), (1280, 800)), view(0, 0, 1280, 800));
    }

    #[test]
    fn estados_que_encerram_a_captura() {
        // Coming up, and going idle while still attached, are not the end.
        assert!(!capture_over(&Unconnected, &Connecting));
        assert!(!capture_over(&Connecting, &Paused));
        assert!(!capture_over(&Paused, &Streaming));
        assert!(!capture_over(&Streaming, &Paused));
        assert!(capture_over(&Streaming, &Unconnected));
        assert!(capture_over(&Paused, &Error("gone".into())));
    }
}
