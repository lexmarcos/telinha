//! Annex B H.264 utilities: find NAL units, build the WebCodecs codec string
//! from the SPS, and ensure SPS/PPS on every keyframe (viewers who join
//! mid-stream decode from it).

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
/// arrives without them.
#[derive(Default)]
pub struct Headers {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl Headers {
    pub fn codec(&self) -> Option<String> {
        self.sps.as_deref().and_then(codec_string)
    }

    pub fn fix_keyframe(&mut self, data: Vec<u8>, key: bool) -> Vec<u8> {
        let nals = nal_units(&data);
        let mut has_sps = false;
        let mut has_pps = false;
        for &n in &nals {
            match nal_type(&data, n) {
                7 => {
                    has_sps = true;
                    self.sps = Some(data[n.0..n.1].to_vec());
                }
                8 => {
                    has_pps = true;
                    self.pps = Some(data[n.0..n.1].to_vec());
                }
                _ => {}
            }
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
        let first = vec![0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x1f, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 1];
        assert_eq!(h.fix_keyframe(first.clone(), true), first);
        let idr = vec![0, 0, 0, 1, 0x65, 2];
        let fixed = h.fix_keyframe(idr, true);
        let types: Vec<u8> = nal_units(&fixed).into_iter().map(|n| nal_type(&fixed, n)).collect();
        assert_eq!(types, vec![7, 8, 5]);
        assert_eq!(h.codec().unwrap(), "avc1.42e01f");
    }
}
