//! Annex B H.264 utilities: find NAL units, build the WebCodecs codec string
//! from the SPS, ensure SPS/PPS on every keyframe (viewers who join
//! mid-stream decode from it) and tell decoders not to hold frames back.

/// Positions (content start, end) of each NAL, without the start code.
pub fn nal_units(data: &[u8]) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut e = starts.get(k + 1).map_or(data.len(), |&n| n - 3);
        // A 4-byte start code leaves an extra zero at the end of the previous one.
        while e > s && data[e - 1] == 0 && k + 1 < starts.len() {
            e -= 1;
        }
        out.push((s, e));
    }
    out
}

pub fn nal_type(data: &[u8], (s, _): (usize, usize)) -> u8 {
    data.get(s).map_or(0, |b| b & 0x1f)
}

/// "avc1.PPCCLL" from the SPS (profile, constraints and level).
pub fn codec_string(sps: &[u8]) -> Option<String> {
    (sps.len() >= 4).then(|| format!("avc1.{:02x}{:02x}{:02x}", sps[1], sps[2], sps[3]))
}

/// Keeps the last seen SPS/PPS and prepends them to any keyframe that
/// arrives without them. Every SPS that goes out is rewritten by
/// [`low_delay_sps`].
#[derive(Default)]
pub struct Headers {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    /// The encoder repeats the same SPS: (as encoded, as sent).
    fixed: Option<(Vec<u8>, Vec<u8>)>,
}

impl Headers {
    pub fn codec(&self) -> Option<String> {
        self.sps.as_deref().and_then(codec_string)
    }

    fn fix_sps(&mut self, raw: &[u8]) -> Vec<u8> {
        if let Some((orig, new)) = &self.fixed {
            if orig == raw {
                return new.clone();
            }
        }
        let new = low_delay_sps(raw).unwrap_or_else(|| raw.to_vec());
        if new != raw {
            tracing::debug!("SPS rewritten to output frames without delay");
        }
        self.fixed = Some((raw.to_vec(), new.clone()));
        new
    }

    pub fn fix_keyframe(&mut self, mut data: Vec<u8>, key: bool) -> Vec<u8> {
        let nals = nal_units(&data);
        let mut has_sps = false;
        let mut has_pps = false;
        let mut changed = false;
        for &n in &nals {
            match nal_type(&data, n) {
                7 => {
                    has_sps = true;
                    let fixed = self.fix_sps(&data[n.0..n.1]);
                    changed |= fixed[..] != data[n.0..n.1];
                    self.sps = Some(fixed);
                }
                8 => {
                    has_pps = true;
                    self.pps = Some(data[n.0..n.1].to_vec());
                }
                _ => {}
            }
        }
        if changed {
            let mut out = Vec::with_capacity(data.len() + 16);
            let mut last = 0;
            for &n in nals.iter().filter(|&&n| nal_type(&data, n) == 7) {
                out.extend_from_slice(&data[last..n.0]);
                out.extend_from_slice(&self.fix_sps(&data[n.0..n.1]));
                last = n.1;
            }
            out.extend_from_slice(&data[last..]);
            data = out;
        }
        if !key || (has_sps && has_pps) {
            return data;
        }
        let mut out = Vec::with_capacity(data.len() + 64);
        for h in [&self.sps, &self.pps].into_iter().flatten() {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(h);
        }
        out.extend_from_slice(&data);
        out
    }
}

/* ---------------- SPS: bitstream_restriction ---------------- */

// Without VUI bitstream_restriction a decoder must assume frames may come out
// of order and can hold up to the level's whole DPB before showing one (at
// 4K, level 5.1/5.2, about 5 frames: ~80 ms at 60 fps). Chrome's WebRTC
// rewrites the SPS for the same reason (sps_vui_rewriter.cc); here it is done
// once, at the source, so the data channel path (WebCodecs) gets it too.

/// The SPS (NAL with its header byte) with max_num_reorder_frames = 0 and
/// max_dec_frame_buffering = max_num_ref_frames (at least 1), every other
/// field untouched. `None` when it already says so or cannot be parsed
/// (then the original goes out as it was).
pub fn low_delay_sps(nal: &[u8]) -> Option<Vec<u8>> {
    let (&header, ebsp) = nal.split_first()?;
    if header & 0x1f != 7 {
        return None;
    }
    let rbsp = unescape(ebsp);
    let sps = parse_sps(&rbsp)?;
    let dec = sps.ref_frames.max(1);
    if sps.restriction.is_some_and(|r| r.reorder == 0 && r.dec_buffering <= dec) {
        return None;
    }
    let mut w = Writer::default();
    let copy = |w: &mut Writer, from: usize, to: usize| {
        for i in from..to {
            w.bit(u32::from(rbsp[i / 8] >> (7 - i % 8) & 1));
        }
    };
    copy(&mut w, 0, sps.vui_flag);
    w.bit(1);
    match sps.vui_end {
        // VUI already there: keep it up to the restriction.
        Some(end) => copy(&mut w, sps.vui_flag + 1, end),
        // No VUI: an empty one (all "present" flags off, nothing inferred changes).
        None => {
            for _ in 0..8 {
                w.bit(0); // aspect, overscan, signal, chroma loc, timing, NAL HRD, VCL HRD, pic_struct
            }
        }
    }
    // Absent fields' inferred values (E.2.1), the same WebRTC writes.
    let r = sps.restriction.unwrap_or(Restriction { mv_over_boundaries: 1, bytes_per_pic: 2, bits_per_mb: 1, mv_h: 16, mv_v: 16, reorder: 0, dec_buffering: 0 });
    w.bit(1);
    w.bit(r.mv_over_boundaries);
    for v in [r.bytes_per_pic, r.bits_per_mb, r.mv_h, r.mv_v, 0, dec] {
        w.ue(v);
    }
    w.trailing();
    let mut out = vec![header];
    out.extend(escape(&w.out));
    // Only what parses back as intended goes out.
    let check = parse_sps(&unescape(&out[1..]))?.restriction?;
    (check.reorder == 0 && check.dec_buffering == dec).then_some(out)
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Restriction {
    mv_over_boundaries: u32,
    bytes_per_pic: u32,
    bits_per_mb: u32,
    mv_h: u32,
    mv_v: u32,
    reorder: u32,
    dec_buffering: u32,
}

#[derive(Debug)]
struct Sps {
    ref_frames: u32,
    /// Bit position of vui_parameters_present_flag.
    vui_flag: usize,
    /// With VUI: bit position of bitstream_restriction_flag.
    vui_end: Option<usize>,
    restriction: Option<Restriction>,
}

/// Parses an SPS RBSP (7.3.2.1.1 and E.1.1) up to the end of the VUI.
fn parse_sps(rbsp: &[u8]) -> Option<Sps> {
    let mut r = Bits { data: rbsp, pos: 0 };
    let profile = r.bits(8)?;
    r.bits(16)?; // constraint flags, level
    if r.ue()? > 31 {
        return None;
    }
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma = r.ue()?;
        if chroma == 3 {
            r.bit()?;
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.bit()?; // qpprime_y_zero_transform_bypass_flag
        if r.bit()? == 1 {
            for i in 0..if chroma != 3 { 8 } else { 12 } {
                if r.bit()? == 1 {
                    scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.bit()?;
            r.se()?;
            r.se()?;
            let n = r.ue()?;
            if n > 255 {
                return None;
            }
            for _ in 0..n {
                r.se()?;
            }
        }
        2 => {}
        _ => return None,
    }
    let ref_frames = r.ue()?;
    r.bit()?; // gaps_in_frame_num_value_allowed_flag
    r.ue()?; // pic_width_in_mbs_minus1
    r.ue()?; // pic_height_in_map_units_minus1
    if r.bit()? == 0 {
        r.bit()?; // mb_adaptive_frame_field_flag
    }
    r.bit()?; // direct_8x8_inference_flag
    if r.bit()? == 1 {
        for _ in 0..4 {
            r.ue()?; // frame_crop_*_offset
        }
    }
    let vui_flag = r.pos;
    let mut sps = Sps { ref_frames, vui_flag, vui_end: None, restriction: None };
    if r.bit()? == 1 {
        if r.bit()? == 1 && r.bits(8)? == 255 {
            r.bits(32)?; // sar_width, sar_height
        }
        if r.bit()? == 1 {
            r.bit()?; // overscan_appropriate_flag
        }
        if r.bit()? == 1 {
            r.bits(4)?; // video_format, video_full_range_flag
            if r.bit()? == 1 {
                r.bits(24)?; // colour_primaries, transfer_characteristics, matrix_coefficients
            }
        }
        if r.bit()? == 1 {
            r.ue()?;
            r.ue()?;
        }
        if r.bit()? == 1 {
            r.bits(32)?;
            r.bits(32)?;
            r.bit()?; // fixed_frame_rate_flag
        }
        let nal_hrd = r.bit()? == 1;
        if nal_hrd {
            hrd(&mut r)?;
        }
        let vcl_hrd = r.bit()? == 1;
        if vcl_hrd {
            hrd(&mut r)?;
        }
        if nal_hrd || vcl_hrd {
            r.bit()?; // low_delay_hrd_flag
        }
        r.bit()?; // pic_struct_present_flag
        sps.vui_end = Some(r.pos);
        if r.bit()? == 1 {
            sps.restriction = Some(Restriction {
                mv_over_boundaries: r.bit()?,
                bytes_per_pic: r.ue()?,
                bits_per_mb: r.ue()?,
                mv_h: r.ue()?,
                mv_v: r.ue()?,
                reorder: r.ue()?,
                dec_buffering: r.ue()?,
            });
        }
    }
    // Only rbsp_trailing_bits may follow: anything else is not understood here.
    if r.bit()? != 1 || (r.pos..rbsp.len() * 8).any(|_| r.bit() != Some(0)) {
        return None;
    }
    Some(sps)
}

fn scaling_list(r: &mut Bits, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i32, 8i32);
    for _ in 0..size {
        if next != 0 {
            next = (last + r.se()? + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

fn hrd(r: &mut Bits) -> Option<()> {
    let count = r.ue()? + 1;
    if count > 32 {
        return None;
    }
    r.bits(8)?; // bit_rate_scale, cpb_size_scale
    for _ in 0..count {
        r.ue()?;
        r.ue()?;
        r.bit()?; // cbr_flag
    }
    r.bits(20)?; // four 5-bit lengths
    Some(())
}

/// Removes emulation prevention bytes (00 00 03 → 00 00).
fn unescape(ebsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ebsp.len());
    let mut zeros = 0;
    for &b in ebsp {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Adds emulation prevention bytes where 00 00 would be followed by 00–03.
fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 4);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let v = byte >> (7 - self.pos % 8) & 1;
        self.pos += 1;
        Some(u32::from(v))
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v: u64 = 0;
        for _ in 0..n {
            v = v << 1 | u64::from(self.bit()?);
        }
        u32::try_from(v).ok()
    }

    /// Exp-Golomb, unsigned.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        u32::try_from((1u64 << zeros) - 1 + u64::from(self.bits(zeros)?)).ok()
    }

    /// Exp-Golomb, signed.
    fn se(&mut self) -> Option<i32> {
        let k = i64::from(self.ue()?);
        i32::try_from(if k & 1 == 1 { (k + 1) / 2 } else { -k / 2 }).ok()
    }
}

#[derive(Default)]
struct Writer {
    out: Vec<u8>,
    len: usize,
}

impl Writer {
    fn bit(&mut self, b: u32) {
        if self.len % 8 == 0 {
            self.out.push(0);
        }
        if b & 1 == 1 {
            *self.out.last_mut().expect("pushed above") |= 0x80 >> (self.len % 8);
        }
        self.len += 1;
    }

    fn ue(&mut self, v: u32) {
        let x = u64::from(v) + 1;
        let n = 64 - x.leading_zeros();
        for _ in 1..n {
            self.bit(0);
        }
        for i in (0..n).rev() {
            self.bit((x >> i & 1) as u32);
        }
    }

    fn trailing(&mut self) {
        self.bit(1);
        while self.len % 8 != 0 {
            self.bit(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acha_nals_e_codec() {
        let s = [0, 0, 0, 1, 0x67, 0x64, 0x00, 0x2a, 0xaa, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88];
        let n = nal_units(&s);
        assert_eq!(n.len(), 3);
        assert_eq!(nal_type(&s, n[0]), 7);
        assert_eq!(nal_type(&s, n[1]), 8);
        assert_eq!(nal_type(&s, n[2]), 5);
        assert_eq!(codec_string(&s[n[0].0..n[0].1]).unwrap(), "avc1.64002a");
    }

    #[test]
    fn poe_cabecalho_na_keyframe() {
        let mut h = Headers::default();
        let sps = hex(BASELINE_720P);
        let mut first = vec![0, 0, 0, 1];
        first.extend(&sps);
        first.extend([0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 1]);
        assert_eq!(h.fix_keyframe(first.clone(), true), first);
        let idr = vec![0, 0, 0, 1, 0x65, 2];
        let fixed = h.fix_keyframe(idr, true);
        let types: Vec<u8> = nal_units(&fixed).into_iter().map(|n| nal_type(&fixed, n)).collect();
        assert_eq!(types, vec![7, 8, 5]);
        assert_eq!(h.codec().unwrap(), "avc1.42c01f");
    }

    // Real SPS NALs, from FFmpeg 6.1 on this project's machines:
    // h264_vaapi (Iris Xe) High 4K CBR with BT.709 and NAL HRD: already says reorder 0.
    const VAAPI_4K: &str = "67640c34ac2b401e0021f602d404040500000300010000030078e6a000262580004c4b2f738078402150";
    // h264_vaapi High 1080p without colour description.
    const VAAPI_1080P: &str = "67640c2aac2b403c0113f2e02d10000003001000000788f08042a0";
    // libx264 High 4K, BT.709, 4 references and max_num_reorder_frames = 2.
    const X264_4K: &str = "67640034acd9403c0043ec05a808080a000003000200000300f01e30632c";
    // libx264 Constrained Baseline 720p.
    const BASELINE_720P: &str = "6742c01fda014016ec0440000003004000000f23c60ca8";

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn parse(nal: &[u8]) -> (Sps, Vec<u8>) {
        let rbsp = unescape(&nal[1..]);
        (parse_sps(&rbsp).expect("SPS parses"), rbsp)
    }

    fn bit_at(rbsp: &[u8], i: usize) -> u8 {
        rbsp[i / 8] >> (7 - i % 8) & 1
    }

    /// The same SPS cut at `pos` (bits before kept), followed by `tail` and
    /// trailing bits: to build the variants the encoders do produce.
    fn splice(nal: &[u8], pos: usize, tail: &[u32]) -> Vec<u8> {
        let rbsp = unescape(&nal[1..]);
        let mut w = Writer::default();
        for i in 0..pos {
            w.bit(u32::from(bit_at(&rbsp, i)));
        }
        for &b in tail {
            w.bit(b);
        }
        w.trailing();
        let mut out = vec![nal[0]];
        out.extend(escape(&w.out));
        out
    }

    /// Rewrites and checks that everything before the restriction is intact.
    fn check(nal: &[u8], refs: u32) {
        let (before, old) = parse(nal);
        let out = low_delay_sps(nal).expect("rewritten");
        let (after, new) = parse(&out);
        let r = after.restriction.expect("restriction present");
        assert_eq!((r.reorder, r.dec_buffering), (0, refs.max(1)));
        assert_eq!(after.ref_frames, refs);
        assert_eq!(after.vui_flag, before.vui_flag);
        for i in 0..before.vui_flag {
            assert_eq!(bit_at(&old, i), bit_at(&new, i), "bit {i}");
        }
        if let Some(end) = before.vui_end {
            assert_eq!(after.vui_end, Some(end));
            for i in before.vui_flag..end {
                assert_eq!(bit_at(&old, i), bit_at(&new, i), "VUI bit {i}");
            }
        }
        if let Some(b) = before.restriction {
            assert_eq!((r.mv_over_boundaries, r.bytes_per_pic, r.bits_per_mb, r.mv_h, r.mv_v), (b.mv_over_boundaries, b.bytes_per_pic, b.bits_per_mb, b.mv_h, b.mv_v));
        }
        assert_eq!(&out[..4], &nal[..4], "profile and level stay the same");
        assert_eq!(low_delay_sps(&out), None, "already fixed");
    }

    #[test]
    fn sps_que_ja_diz_sem_atraso_fica_igual() {
        assert_eq!(low_delay_sps(&hex(VAAPI_4K)), None);
        assert_eq!(low_delay_sps(&hex(VAAPI_1080P)), None);
        assert_eq!(low_delay_sps(&hex(BASELINE_720P)), None);
        let (sps, _) = parse(&hex(VAAPI_4K));
        assert_eq!(sps.restriction.map(|r| (r.reorder, r.dec_buffering)), Some((0, 1)));
    }

    #[test]
    fn corrige_reordenacao_existente() {
        let (sps, _) = parse(&hex(X264_4K));
        assert_eq!(sps.restriction.map(|r| (r.reorder, r.dec_buffering)), Some((2, 4)));
        check(&hex(X264_4K), 4);
    }

    #[test]
    fn acrescenta_restricao_ao_vui_sem_ela() {
        // NVENC-like: VUI with colour info and HRD, but no bitstream_restriction.
        for s in [VAAPI_4K, VAAPI_1080P, X264_4K, BASELINE_720P] {
            let nal = hex(s);
            let (sps, _) = parse(&nal);
            let stripped = splice(&nal, sps.vui_end.unwrap(), &[0]);
            let (st, _) = parse(&stripped);
            assert!(st.restriction.is_none());
            check(&stripped, sps.ref_frames);
        }
    }

    #[test]
    fn cria_vui_quando_nao_ha() {
        // OpenH264-like: no VUI at all.
        for s in [VAAPI_4K, X264_4K, BASELINE_720P] {
            let nal = hex(s);
            let (sps, _) = parse(&nal);
            let bare = splice(&nal, sps.vui_flag, &[0]);
            let (b, _) = parse(&bare);
            assert!(b.vui_end.is_none());
            check(&bare, sps.ref_frames);
            let (after, _) = parse(&low_delay_sps(&bare).unwrap());
            assert!(after.vui_end.is_some());
        }
    }

    #[test]
    fn sps_estranho_passa_intacto() {
        assert_eq!(low_delay_sps(&[0x67]), None);
        assert_eq!(low_delay_sps(&[0x67, 0x64, 0x00]), None);
        assert_eq!(low_delay_sps(&hex(VAAPI_4K)[..10]), None);
        assert_eq!(low_delay_sps(&[0x68, 0xce, 0x3c, 0x80]), None, "a PPS is not touched");
        // Unknown bits after the VUI: left alone.
        let nal = hex(X264_4K);
        let (sps, _) = parse(&nal);
        let odd = splice(&nal, sps.vui_end.unwrap(), &[0, 1, 1]);
        assert!(parse_sps(&unescape(&odd[1..])).is_none());
        assert_eq!(low_delay_sps(&odd), None);
    }

    #[test]
    fn prevencao_de_emulacao() {
        let raw = [0x12, 0, 0, 0, 0, 0, 1, 0, 0, 3, 0, 0, 2, 0xff];
        let esc = escape(&raw);
        assert_eq!(esc, [0x12, 0, 0, 3, 0, 0, 3, 0, 1, 0, 0, 3, 3, 0, 0, 3, 2, 0xff]);
        assert_eq!(unescape(&esc), raw);
    }

    #[test]
    fn keyframe_sai_com_sps_corrigido() {
        let mut h = Headers::default();
        let sps = hex(X264_4K);
        let mut frame = vec![0, 0, 0, 1];
        frame.extend(&sps);
        frame.extend([0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88, 0x84]);
        let out = h.fix_keyframe(frame, true);
        let nals = nal_units(&out);
        let types: Vec<u8> = nals.iter().map(|&n| nal_type(&out, n)).collect();
        assert_eq!(types, vec![7, 8, 5]);
        let new_sps = &out[nals[0].0..nals[0].1];
        assert_eq!(new_sps, low_delay_sps(&sps).unwrap());
        assert_eq!(&out[nals[2].0..], &[0x65, 0x88, 0x84]);
        assert_eq!(h.codec().unwrap(), "avc1.640034");
        // An IDR without headers gets the corrected one.
        let idr = h.fix_keyframe(vec![0, 0, 0, 1, 0x65, 1], true);
        let n = nal_units(&idr);
        assert_eq!(&idr[n[0].0..n[0].1], new_sps);
    }
}
