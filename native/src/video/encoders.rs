//! Encoder candidates, best to worst, with the low-latency options Sunshine
//! uses for each one (src/video.cpp and src/nvenc/): no B-frames, "infinite"
//! GOP with IDR only on demand, constant bitrate and each vendor's
//! "ultra low latency" modes.

use ffmpeg_sys_next::AVHWDeviceType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// BGRX frames coming from the CPU (PipeWire, test pattern).
    Cpu,
    /// BGRX frames already on the GPU (PipeWire DMA-BUF, Linux).
    Prime,
    /// Windows desktop capture inside FFmpeg itself (ddagrab).
    Desktop,
}

pub struct Ctx {
    pub native: (u32, u32),
    pub out: (u32, u32),
    pub fps: u32,
}

impl Ctx {
    fn scaled(&self) -> bool {
        self.native != self.out
    }
}

pub struct Candidate {
    /// Human-readable name ("NVENC").
    pub label: &'static str,
    pub encoder: &'static str,
    pub hardware: bool,
    pub device: Option<(AVHWDeviceType, Option<&'static str>)>,
    pub source: SourceKind,
    pub chain: fn(&Ctx) -> String,
    pub options: fn(&Ctx) -> Vec<(&'static str, String)>,
}

fn o(k: &'static str, v: &str) -> (&'static str, String) {
    (k, v.to_owned())
}

fn nvenc_opts(_: &Ctx) -> Vec<(&'static str, String)> {
    vec![
        o("preset", "p1"),
        o("tune", "ull"),
        o("zerolatency", "1"),
        o("delay", "0"),
        o("forced-idr", "1"),
        o("rc", "cbr"),
        o("profile", "high"),
        o("surfaces", "1"),
    ]
}

fn amf_opts(_: &Ctx) -> Vec<(&'static str, String)> {
    vec![
        o("usage", "ultralowlatency"),
        o("quality", "speed"),
        o("rc", "cbr"),
        o("forced_idr", "1"),
        o("header_insertion_mode", "idr"),
        o("latency", "1"),
        o("async_depth", "1"),
        o("preencode", "0"),
        o("enforce_hrd", "1"),
    ]
}

fn qsv_opts(_: &Ctx) -> Vec<(&'static str, String)> {
    vec![o("preset", "veryfast"), o("async_depth", "1"), o("low_delay_brc", "1"), o("forced_idr", "1"), o("look_ahead", "0")]
}

fn vaapi_opts(_: &Ctx) -> Vec<(&'static str, String)> {
    vec![o("rc_mode", "CBR"), o("async_depth", "1"), o("profile", "high"), o("low_power", "1"), o("sei", "0"), o("aud", "0")]
}

fn vaapi_opts_full_power(c: &Ctx) -> Vec<(&'static str, String)> {
    let mut v = vaapi_opts(c);
    v.retain(|(k, _)| *k != "low_power");
    v
}

fn mf_opts(_: &Ctx) -> Vec<(&'static str, String)> {
    vec![o("hw_encoding", "1"), o("scenario", "display_remoting"), o("rate_control", "cbr")]
}

fn openh264_opts(_: &Ctx) -> Vec<(&'static str, String)> {
    vec![o("allow_skip_frames", "0"), o("rc_mode", "bitrate"), o("profile", "constrained_baseline")]
}

fn cpu_scale(c: &Ctx) -> String {
    if c.scaled() { format!("scale={}:{}:flags=fast_bilinear", c.out.0, c.out.1) } else { String::new() }
}

fn join(parts: &[String]) -> String {
    parts.iter().filter(|p| !p.is_empty()).cloned().collect::<Vec<_>>().join(",")
}

#[cfg(target_os = "windows")]
fn desktop(c: &Ctx) -> String {
    format!("ddagrab=output_idx=0:framerate={}:draw_mouse=1", c.fps)
}

#[cfg(target_os = "windows")]
fn d3d11_scale(c: &Ctx, nv12: bool) -> String {
    match (c.scaled(), nv12) {
        (true, _) => format!("scale_d3d11=width={}:height={}:format=nv12", c.out.0, c.out.1),
        (false, true) => "scale_d3d11=format=nv12".into(),
        (false, false) => String::new(),
    }
}

pub fn candidates() -> Vec<Candidate> {
    let mut v = Vec::new();

    #[cfg(target_os = "windows")]
    {
        // The image starts on the GPU (Desktop Duplication) and goes straight to the encoder.
        v.push(Candidate {
            label: "NVENC",
            encoder: "h264_nvenc",
            hardware: true,
            device: None,
            source: SourceKind::Desktop,
            chain: |c| join(&[desktop(c), d3d11_scale(c, false)]),
            options: nvenc_opts,
        });
        v.push(Candidate {
            label: "AMF",
            encoder: "h264_amf",
            hardware: true,
            device: None,
            source: SourceKind::Desktop,
            chain: |c| join(&[desktop(c), d3d11_scale(c, true)]),
            options: amf_opts,
        });
        v.push(Candidate {
            label: "Quick Sync",
            encoder: "h264_qsv",
            hardware: true,
            device: None,
            source: SourceKind::Desktop,
            chain: |c| join(&[desktop(c), d3d11_scale(c, true), "hwmap=derive_device=qsv".into(), "format=qsv".into()]),
            options: qsv_opts,
        });
        v.push(Candidate {
            label: "Media Foundation",
            encoder: "h264_mf",
            hardware: true,
            device: None,
            source: SourceKind::Desktop,
            chain: |c| join(&[desktop(c), d3d11_scale(c, true)]),
            options: mf_opts,
        });
        // If the GPU conversion (scale_d3d11) fails, as on some cards and
        // drivers: the image is downloaded to memory, converted to NV12 there and
        // fed to the card's encoder. Costs one copy per frame, but still encodes on the GPU.
        for (label, encoder, options) in [
            ("AMF", "h264_amf", amf_opts as fn(&Ctx) -> Vec<_>),
            ("Quick Sync", "h264_qsv", qsv_opts),
            ("Media Foundation", "h264_mf", mf_opts),
        ] {
            v.push(Candidate {
                label,
                encoder,
                hardware: true,
                device: None,
                source: SourceKind::Desktop,
                chain: |c| join(&[desktop(c), "hwdownload".into(), "format=bgra".into(), cpu_scale(c), "format=nv12".into()]),
                options,
            });
        }
        v.push(Candidate {
            label: "OpenH264",
            encoder: "libopenh264",
            hardware: false,
            device: None,
            source: SourceKind::Desktop,
            chain: |c| join(&[desktop(c), "hwdownload".into(), "format=bgra".into(), cpu_scale(c), "format=yuv420p".into()]),
            options: openh264_opts,
        });
    }

    #[cfg(target_os = "linux")]
    {
        // Uploads the image to the GPU and converts/scales there (VPP), like Sunshine.
        v.push(Candidate {
            label: "VAAPI",
            encoder: "h264_vaapi",
            hardware: true,
            device: Some((AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, None)),
            source: SourceKind::Cpu,
            chain: |c| format!("hwupload,scale_vaapi=w={}:h={}:format=nv12", c.out.0, c.out.1),
            options: vaapi_opts,
        });
        v.push(Candidate {
            label: "VAAPI",
            encoder: "h264_vaapi",
            hardware: true,
            device: Some((AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, None)),
            source: SourceKind::Cpu,
            chain: |c| join(&[cpu_scale(c), "format=nv12".into(), "hwupload".into()]),
            options: vaapi_opts_full_power,
        });
    }

    #[cfg(target_os = "linux")]
    {
        // Image already on the GPU: VAAPI imports the compositor's buffer and
        // converts/scales right there. Nothing goes through the CPU (like Sunshine with KMS).
        for (opts, label) in [(vaapi_opts as fn(&Ctx) -> Vec<_>, "VAAPI"), (vaapi_opts_full_power, "VAAPI")] {
            v.push(Candidate {
                label,
                encoder: "h264_vaapi",
                hardware: true,
                device: None,
                source: SourceKind::Prime,
                chain: |c| format!("hwmap=derive_device=vaapi,scale_vaapi=w={}:h={}:format=nv12", c.out.0, c.out.1),
                options: opts,
            });
        }
        // No VAAPI (NVIDIA): download the buffer to memory and continue as before.
        v.push(Candidate {
            label: "NVENC",
            encoder: "h264_nvenc",
            hardware: true,
            device: None,
            source: SourceKind::Prime,
            chain: |c| join(&["hwdownload".into(), "format=bgr0".into(), cpu_scale(c)]),
            options: nvenc_opts,
        });
        v.push(Candidate {
            label: "OpenH264",
            encoder: "libopenh264",
            hardware: false,
            device: None,
            source: SourceKind::Prime,
            chain: |c| join(&["hwdownload".into(), "format=bgr0".into(), cpu_scale(c), "format=yuv420p".into()]),
            options: openh264_opts,
        });
    }

    // NVIDIA cards accept BGRX straight from the CPU and convert on the GPU.
    v.push(Candidate {
        label: "NVENC",
        encoder: "h264_nvenc",
        hardware: true,
        device: None,
        source: SourceKind::Cpu,
        chain: cpu_scale,
        options: nvenc_opts,
    });
    v.push(Candidate {
        label: "OpenH264",
        encoder: "libopenh264",
        hardware: false,
        device: None,
        source: SourceKind::Cpu,
        chain: |c| join(&[cpu_scale(c), "format=yuv420p".into()]),
        options: openh264_opts,
    });
    v
}
