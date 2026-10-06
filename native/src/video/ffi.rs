//! Camada fina sobre o FFmpeg: dispositivo de hardware, grafo de filtros e
//! codificador. Cada tipo libera o que é dele no `Drop`.

use std::ffi::{CStr, CString};
use std::ptr;

use ffmpeg_sys_next as ff;

#[derive(Debug, Clone)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn av_err(code: i32) -> String {
    let mut buf = [0 as std::os::raw::c_char; 256];
    unsafe {
        ff::av_strerror(code, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

fn check(ret: i32, what: &str) -> Result<i32> {
    if ret < 0 { Err(Error(format!("{what}: {}", av_err(ret)))) } else { Ok(ret) }
}

fn cstr(s: &str) -> CString {
    CString::new(s).expect("texto sem zero no meio")
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    #[link_name = "close"]
    fn libc_close(fd: i32) -> i32;
}

/// AVERROR(EAGAIN): EAGAIN é 11 no Linux e no CRT do Windows.
const EAGAIN: i32 = -11;

/* ---------------- dispositivo de hardware ---------------- */

pub struct HwDevice(*mut ff::AVBufferRef);

unsafe impl Send for HwDevice {}

impl HwDevice {
    pub fn create(kind: ff::AVHWDeviceType, device: Option<&str>) -> Result<Self> {
        let mut ctx = ptr::null_mut();
        let dev = device.map(cstr);
        let ret = unsafe { ff::av_hwdevice_ctx_create(&mut ctx, kind, dev.as_ref().map_or(ptr::null(), |d| d.as_ptr()), ptr::null_mut(), 0) };
        check(ret, "dispositivo de vídeo")?;
        Ok(Self(ctx))
    }

    pub fn as_ptr(&self) -> *mut ff::AVBufferRef {
        self.0
    }
}

/// Contexto de quadros na GPU (hoje só o de DMA-BUF do Linux: quadros que o
/// compositor já deixou na placa e o FFmpeg só referencia).
pub struct HwFrames(*mut ff::AVBufferRef);

unsafe impl Send for HwFrames {}

impl HwFrames {
    #[cfg(target_os = "linux")]
    pub fn drm_prime(device: &HwDevice, sw_format: ff::AVPixelFormat, width: u32, height: u32) -> Result<Self> {
        unsafe {
            let mut r = ff::av_hwframe_ctx_alloc(device.as_ptr());
            if r.is_null() {
                return Err(Error("sem memória para os quadros da GPU".into()));
            }
            let fc = &mut *((*r).data as *mut ff::AVHWFramesContext);
            fc.format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME;
            fc.sw_format = sw_format;
            fc.width = width as i32;
            fc.height = height as i32;
            let ret = ff::av_hwframe_ctx_init(r);
            if ret < 0 {
                ff::av_buffer_unref(&mut r);
                return Err(Error(format!("quadros DMA-BUF: {}", av_err(ret))));
            }
            Ok(Self(r))
        }
    }

    pub fn as_ptr(&self) -> *mut ff::AVBufferRef {
        self.0
    }
}

impl Drop for HwFrames {
    fn drop(&mut self) {
        unsafe { ff::av_buffer_unref(&mut self.0) };
    }
}

impl Drop for HwDevice {
    fn drop(&mut self) {
        unsafe { ff::av_buffer_unref(&mut self.0) };
    }
}

/* ---------------- quadro ---------------- */

pub struct Frame(pub *mut ff::AVFrame);

unsafe impl Send for Frame {}

impl Frame {
    pub fn new() -> Self {
        Self(unsafe { ff::av_frame_alloc() })
    }

    /// Embrulha uma imagem da CPU (4 bytes por pixel) sem copiar: o FFmpeg fica dono do Vec.
    pub fn from_cpu(data: Vec<u8>, width: u32, height: u32, stride: u32, format: ff::AVPixelFormat, pts: i64) -> Self {
        unsafe extern "C" fn free_vec(opaque: *mut std::ffi::c_void, _: *mut u8) {
            drop(unsafe { Box::from_raw(opaque as *mut Vec<u8>) });
        }
        let frame = Self::new();
        let boxed = Box::new(data);
        let len = boxed.len();
        let data_ptr = boxed.as_ptr() as *mut u8;
        unsafe {
            let f = &mut *frame.0;
            f.format = format as i32;
            f.width = width as i32;
            f.height = height as i32;
            f.pts = pts;
            f.data[0] = data_ptr;
            f.linesize[0] = stride as i32;
            f.buf[0] = ff::av_buffer_create(data_ptr, len, Some(free_vec), Box::into_raw(boxed) as *mut _, 0);
        }
        frame
    }

    /// Embrulha um buffer DMA-BUF de uma camada só (BGRX/RGBX). O FFmpeg fica
    /// dono do fd e fecha quando o último uso acabar.
    #[cfg(target_os = "linux")]
    pub fn from_dmabuf(
        fd: std::os::fd::OwnedFd,
        size: u32,
        offset: u32,
        stride: u32,
        modifier: u64,
        sw_format: ff::AVPixelFormat,
        frames: &HwFrames,
        (width, height): (u32, u32),
        pts: i64,
    ) -> Self {
        use std::os::fd::IntoRawFd;
        unsafe extern "C" fn free_desc(_: *mut std::ffi::c_void, data: *mut u8) {
            let d = data as *mut ff::AVDRMFrameDescriptor;
            unsafe {
                libc_close((*d).objects[0].fd);
                ff::av_free(d as *mut _);
            }
        }
        // Códigos de formato do DRM (drm_fourcc.h): memória B,G,R,X = XR24; R,G,B,X = XB24.
        let fourcc = |c: &[u8; 4]| u32::from_le_bytes(*c);
        let drm_format = if sw_format == ff::AVPixelFormat::AV_PIX_FMT_RGB0 { fourcc(b"XB24") } else { fourcc(b"XR24") };
        let frame = Self::new();
        unsafe {
            let d = ff::av_mallocz(std::mem::size_of::<ff::AVDRMFrameDescriptor>()) as *mut ff::AVDRMFrameDescriptor;
            let desc = &mut *d;
            desc.nb_objects = 1;
            desc.objects[0].fd = fd.into_raw_fd();
            desc.objects[0].size = size as usize;
            desc.objects[0].format_modifier = modifier;
            desc.nb_layers = 1;
            desc.layers[0].format = drm_format;
            desc.layers[0].nb_planes = 1;
            desc.layers[0].planes[0].object_index = 0;
            desc.layers[0].planes[0].offset = offset as isize;
            desc.layers[0].planes[0].pitch = stride as isize;
            let f = &mut *frame.0;
            f.format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            f.width = width as i32;
            f.height = height as i32;
            f.pts = pts;
            f.data[0] = d as *mut u8;
            f.buf[0] = ff::av_buffer_create(d as *mut u8, std::mem::size_of::<ff::AVDRMFrameDescriptor>(), Some(free_desc), ptr::null_mut(), 0);
            f.hw_frames_ctx = ff::av_buffer_ref(frames.as_ptr());
        }
        frame
    }

    pub fn unref(&mut self) {
        unsafe { ff::av_frame_unref(self.0) };
    }

    /// Outra referência à mesma imagem (sem copiar os pixels).
    pub fn share(&self) -> Option<Self> {
        let f = unsafe { ff::av_frame_clone(self.0) };
        (!f.is_null()).then_some(Self(f))
    }

    pub fn set_pts(&mut self, pts: i64) {
        unsafe { (*self.0).pts = pts };
    }

    pub fn pts(&self) -> i64 {
        unsafe { (*self.0).pts }
    }

    pub fn size(&self) -> (u32, u32) {
        unsafe { ((*self.0).width as u32, (*self.0).height as u32) }
    }

    pub fn set_keyframe(&mut self, key: bool) {
        unsafe {
            (*self.0).pict_type = if key { ff::AVPictureType::AV_PICTURE_TYPE_I } else { ff::AVPictureType::AV_PICTURE_TYPE_NONE };
        }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { ff::av_frame_free(&mut self.0) };
    }
}

/* ---------------- grafo de filtros ---------------- */

pub struct InputSpec<'a> {
    pub width: u32,
    pub height: u32,
    pub pix_fmt: ff::AVPixelFormat,
    pub fps: u32,
    /// Quadros que já chegam na GPU (DMA-BUF).
    pub hw_frames: Option<&'a HwFrames>,
}

pub struct Graph {
    graph: *mut ff::AVFilterGraph,
    src: *mut ff::AVFilterContext,
    sink: *mut ff::AVFilterContext,
}

unsafe impl Send for Graph {}

impl Graph {
    /// `spec` é a cadeia de filtros (ex.: "scale=1280:720,format=nv12,hwupload").
    /// Com `input`, a entrada vem de quadros empurrados; sem, a própria cadeia
    /// gera os quadros (ex.: "ddagrab=...").
    pub fn new(spec: &str, input: Option<&InputSpec>, device: Option<&HwDevice>) -> Result<Self> {
        unsafe {
            let graph = ff::avfilter_graph_alloc();
            if graph.is_null() {
                return Err(Error("sem memória para o grafo".into()));
            }
            let mut me = Self { graph, src: ptr::null_mut(), sink: ptr::null_mut() };

            if let Some(i) = input {
                let args = cstr(&format!(
                    "video_size={}x{}:pix_fmt={}:time_base=1/1000000:pixel_aspect=1/1:frame_rate={}/1",
                    i.width, i.height, i.pix_fmt as i32, i.fps
                ));
                // Entrada na GPU: o contexto de quadros tem que estar lá antes de inicializar.
                me.src = ff::avfilter_graph_alloc_filter(graph, ff::avfilter_get_by_name(c"buffer".as_ptr()), c"in".as_ptr());
                if me.src.is_null() {
                    return Err(Error("entrada do grafo: sem memória".into()));
                }
                if let Some(frames) = i.hw_frames {
                    let par = ff::av_buffersrc_parameters_alloc();
                    (*par).format = i.pix_fmt as i32;
                    (*par).hw_frames_ctx = frames.as_ptr();
                    let ret = ff::av_buffersrc_parameters_set(me.src, par);
                    ff::av_free(par as *mut _);
                    check(ret, "entrada na GPU")?;
                }
                check(ff::avfilter_init_str(me.src, args.as_ptr()), "entrada do grafo")?;
            }
            check(
                ff::avfilter_graph_create_filter(&mut me.sink, ff::avfilter_get_by_name(c"buffersink".as_ptr()), c"out".as_ptr(), ptr::null(), ptr::null_mut(), graph),
                "saída do grafo",
            )?;

            // Montagem por etapas: os filtros que sobem imagem pra GPU (hwupload)
            // exigem o dispositivo antes de serem inicializados.
            let body = if spec.is_empty() { "null" } else { spec };
            let spec = cstr(&if me.src.is_null() { format!("{body}[out]") } else { format!("[in]{body}[out]") });
            let mut seg = ptr::null_mut();
            let mut ret = ff::avfilter_graph_segment_parse(graph, spec.as_ptr(), 0, &mut seg);
            if ret >= 0 {
                ret = ff::avfilter_graph_segment_create_filters(seg, 0);
            }
            if ret >= 0 {
                if let Some(dev) = device {
                    for k in 0..(*graph).nb_filters as usize {
                        let f = *(*graph).filters.add(k);
                        if (*f).hw_device_ctx.is_null() {
                            (*f).hw_device_ctx = ff::av_buffer_ref(dev.as_ptr());
                        }
                    }
                }
                ret = ff::avfilter_graph_segment_apply_opts(seg, 0);
            }
            if ret >= 0 {
                ret = ff::avfilter_graph_segment_init(seg, 0);
            }
            // O link devolve as pontas soltas ([in] e [out]); ligamos à entrada e à saída.
            let mut free_in = ptr::null_mut();
            let mut free_out = ptr::null_mut();
            if ret >= 0 {
                ret = ff::avfilter_graph_segment_link(seg, 0, &mut free_in, &mut free_out);
            }
            let mut cur = free_in;
            while ret >= 0 && !cur.is_null() {
                if !me.src.is_null() {
                    ret = ff::avfilter_link(me.src, 0, (*cur).filter_ctx, (*cur).pad_idx as u32);
                }
                cur = (*cur).next;
            }
            let mut cur = free_out;
            while ret >= 0 && !cur.is_null() {
                ret = ff::avfilter_link((*cur).filter_ctx, (*cur).pad_idx as u32, me.sink, 0);
                cur = (*cur).next;
            }
            ff::avfilter_graph_segment_free(&mut seg);
            ff::avfilter_inout_free(&mut free_in);
            ff::avfilter_inout_free(&mut free_out);
            check(ret, "montar filtros")?;
            check(ff::avfilter_graph_config(graph, ptr::null_mut()), "configurar filtros")?;
            Ok(me)
        }
    }

    pub fn push(&mut self, frame: &mut Frame) -> Result<()> {
        unsafe { check(ff::av_buffersrc_add_frame_flags(self.src, frame.0, 0), "entregar quadro").map(|_| ()) }
    }

    /// Tira um quadro pronto. `Ok(false)` quando ainda não tem.
    pub fn pull(&mut self, out: &mut Frame) -> Result<bool> {
        let ret = unsafe { ff::av_buffersink_get_frame(self.sink, out.0) };
        if ret == EAGAIN || ret == ff::AVERROR_EOF {
            return Ok(false);
        }
        check(ret, "tirar quadro")?;
        Ok(true)
    }

    pub fn out_format(&self) -> ff::AVPixelFormat {
        unsafe { std::mem::transmute::<i32, ff::AVPixelFormat>(ff::av_buffersink_get_format(self.sink)) }
    }

    pub fn out_size(&self) -> (u32, u32) {
        unsafe { (ff::av_buffersink_get_w(self.sink) as u32, ff::av_buffersink_get_h(self.sink) as u32) }
    }

    pub fn out_hw_frames(&self) -> *mut ff::AVBufferRef {
        unsafe { ff::av_buffersink_get_hw_frames_ctx(self.sink) }
    }
}

impl Drop for Graph {
    fn drop(&mut self) {
        unsafe { ff::avfilter_graph_free(&mut self.graph) };
    }
}

/* ---------------- codificador ---------------- */

pub struct EncoderParams<'a> {
    pub name: &'a str,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub pix_fmt: ff::AVPixelFormat,
    pub hw_frames: *mut ff::AVBufferRef,
    pub options: &'a [(&'a str, String)],
}

pub struct Packet {
    pub data: Vec<u8>,
    pub key: bool,
    pub pts: i64,
}

pub struct Encoder {
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
}

unsafe impl Send for Encoder {}

impl Encoder {
    pub fn open(p: &EncoderParams) -> Result<Self> {
        unsafe {
            let name = cstr(p.name);
            let codec = ff::avcodec_find_encoder_by_name(name.as_ptr());
            if codec.is_null() {
                return Err(Error(format!("{} não existe nesta build", p.name)));
            }
            let ctx = ff::avcodec_alloc_context3(codec);
            let me = Self { ctx, pkt: ff::av_packet_alloc() };
            let c = &mut *ctx;
            c.width = p.width as i32;
            c.height = p.height as i32;
            c.pix_fmt = p.pix_fmt;
            c.time_base = ff::AVRational { num: 1, den: p.fps as i32 };
            c.framerate = ff::AVRational { num: p.fps as i32, den: 1 };
            // GOP "infinito": keyframe só quando alguém pede (como o Sunshine).
            c.gop_size = (p.fps * 600) as i32;
            c.keyint_min = c.gop_size;
            c.max_b_frames = 0;
            c.bit_rate = p.bitrate as i64;
            c.rc_max_rate = p.bitrate as i64;
            // Buffer de taxa de 1/4 de segundo. O Sunshine usa um quadro só, bom
            // para jogo, mas numa tela parada o codificador (o da Intel
            // principalmente) nunca sobra bits para refinar a imagem: o texto fica
            // borrado para sempre (~26 dB numa área de trabalho 4K, contra ~50 dB
            // assim). O preço é uma rajada maior na keyframe.
            c.rc_buffer_size = (p.bitrate / 4) as i32;
            c.flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
            c.color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
            c.colorspace = ff::AVColorSpace::AVCOL_SPC_BT709;
            c.color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
            c.color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            if !p.hw_frames.is_null() {
                c.hw_frames_ctx = ff::av_buffer_ref(p.hw_frames);
            }
            for (k, v) in p.options {
                let (k, v) = (cstr(k), cstr(v));
                let ret = ff::av_opt_set(c.priv_data, k.as_ptr(), v.as_ptr(), 0);
                if ret < 0 {
                    tracing::debug!(encoder = p.name, opcao = ?k, "opção ignorada: {}", av_err(ret));
                }
            }
            check(ff::avcodec_open2(ctx, codec, ptr::null_mut()), &format!("abrir {}", p.name))?;
            Ok(me)
        }
    }

    pub fn send(&mut self, frame: &mut Frame) -> Result<()> {
        unsafe { check(ff::avcodec_send_frame(self.ctx, frame.0), "codificar").map(|_| ()) }
    }

    pub fn receive(&mut self) -> Result<Option<Packet>> {
        unsafe {
            let ret = ff::avcodec_receive_packet(self.ctx, self.pkt);
            if ret == EAGAIN || ret == ff::AVERROR_EOF {
                return Ok(None);
            }
            check(ret, "receber pacote")?;
            let p = &*self.pkt;
            let data = std::slice::from_raw_parts(p.data, p.size as usize).to_vec();
            let out = Packet { data, key: p.flags & ff::AV_PKT_FLAG_KEY as i32 != 0, pts: p.pts };
            ff::av_packet_unref(self.pkt);
            Ok(Some(out))
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_packet_free(&mut self.pkt);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}

pub fn silence_logs() {
    unsafe { ff::av_log_set_level(ff::AV_LOG_ERROR as i32) };
}
