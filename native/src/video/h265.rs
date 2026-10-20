//! Annex B HEVC utilities: the WebCodecs codec string from the SPS, VPS/SPS/PPS
//! on every keyframe (viewers who join mid-stream decode from it), and a check
//! that the encoder promises no reordering.
//!
//! HEVC needs no SPS rewrite like H.264's (h264::low_delay_sps): how many
//! frames a decoder may hold back (sps_max_num_reorder_pics) is a mandatory SPS
//! field, not an optional VUI one, and encoders without B-frames write 0. That
//! is checked here and logged if a driver ever writes otherwise.

use super::h264::{Bits, nal_units, unescape};

fn nal_type(data: &[u8], (s, _): (usize, usize)) -> u8 {
    data.get(s).map_or(0, |b| (b >> 1) & 0x3f)
}

const VPS: u8 = 32;
const SPS: u8 = 33;
const PPS: u8 = 34;

/// "hvc1.P.C.TL.B…" (ISO/IEC 14496-15 annex E) from the SPS NAL, header included.
pub fn codec_string(sps: &[u8]) -> Option<String> {
    let rbsp = unescape(sps.get(2..)?);
    let mut r = Bits::new(&rbsp);
    r.bits(4)?; // sps_video_parameter_set_id
    r.bits(3)?; // sps_max_sub_layers_minus1
    r.bits(1)?; // sps_temporal_id_nesting_flag
    let space = r.bits(2)?;
    let tier = r.bits(1)?;
    let profile = r.bits(5)?;
    let compat = r.bits(32)?;
    let mut constraints = [0u8; 6];
    for c in &mut constraints {
        *c = r.bits(8)? as u8;
    }
    let level = r.bits(8)?;
    let space = ["", "A", "B", "C"][space as usize];
    let mut s = format!("hvc1.{space}{profile}.{:X}.{}{level}", compat.reverse_bits(), if tier == 1 { 'H' } else { 'L' });
    let used = constraints.iter().rposition(|&c| c != 0).map_or(0, |i| i + 1);
    for c in &constraints[..used] {
        s.push_str(&format!(".{c:X}"));
    }
    Some(s)
}

/// sps_max_num_reorder_pics of the highest sub-layer, when the SPS is simple
/// enough to read (one sub-layer, as every encoder here makes).
pub fn max_reorder(sps: &[u8]) -> Option<u32> {
    let rbsp = unescape(sps.get(2..)?);
    let mut r = Bits::new(&rbsp);
    r.bits(4)?;
    if r.bits(3)? != 0 {
        return None; // sub-layers: their profile/level entries come first
    }
    r.bits(1)?;
    for _ in 0..12 {
        r.bits(8)?; // general profile_tier_level
    }
    r.ue()?; // sps_seq_parameter_set_id
    if r.ue()? == 3 {
        r.bits(1)?; // separate_colour_plane_flag
    }
    r.ue()?; // pic_width_in_luma_samples
    r.ue()?; // pic_height_in_luma_samples
    if r.bits(1)? == 1 {
        for _ in 0..4 {
            r.ue()?; // conformance window
        }
    }
    r.ue()?; // bit_depth_luma_minus8
    r.ue()?; // bit_depth_chroma_minus8
    r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
    r.bits(1)?; // sps_sub_layer_ordering_info_present_flag
    r.ue()?; // sps_max_dec_pic_buffering_minus1
    r.ue() // sps_max_num_reorder_pics
}

/// Keeps the last VPS/SPS/PPS and prepends them to any keyframe that arrives
/// without them.
#[derive(Default)]
pub struct Headers {
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    checked: bool,
}

impl Headers {
    pub fn codec(&self) -> Option<String> {
        self.sps.as_deref().and_then(codec_string)
    }

    pub fn fix_keyframe(&mut self, data: Vec<u8>, key: bool) -> Vec<u8> {
        let nals = nal_units(&data);
        let mut has = [false; 3];
        for &n in &nals {
            let slot = match nal_type(&data, n) {
                VPS => 0,
                SPS => 1,
                PPS => 2,
                _ => continue,
            };
            has[slot] = true;
            let nal = data[n.0..n.1].to_vec();
            if slot == 1 && !self.checked {
                self.checked = true;
                match max_reorder(&nal) {
                    Some(0) => {}
                    other => tracing::warn!(reorder = ?other, "HEVC SPS allows reordering: decoders may hold frames back"),
                }
            }
            *[&mut self.vps, &mut self.sps, &mut self.pps][slot] = Some(nal);
        }
        if !key || has.iter().all(|&h| h) {
            return data;
        }
        let mut out = Vec::with_capacity(data.len() + 128);
        for h in [&self.vps, &self.sps, &self.pps].into_iter().flatten() {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(h);
        }
        out.extend_from_slice(&data);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // SPS from hevc_vaapi on an Intel iGPU (Iris Xe), 1280x720 Main, level 3.1.
    const SPS_VAAPI: [u8; 48] = [
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xb0, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x5d, 0xa0, 0x02, 0x80, 0x80, 0x2d, 0x16,
        0x24, 0xb9, 0x24, 0x6d, 0xaa, 0x6a, 0x02, 0x02, 0x02, 0x08, 0x00, 0x00, 0x03, 0x00, 0x08, 0x00, 0x00, 0x03, 0x01, 0xe6, 0xbe, 0x10, 0x08, 0x20,
    ];

    #[test]
    fn texto_do_codec() {
        assert_eq!(codec_string(&SPS_VAAPI).as_deref(), Some("hvc1.1.6.L93.B0"));
        assert_eq!(max_reorder(&SPS_VAAPI), Some(0));
    }
}
