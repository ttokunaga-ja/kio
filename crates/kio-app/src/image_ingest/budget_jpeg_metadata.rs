//! Decoder-owned encoded metadata of locked zune-jpeg 0.5.15.
//! Executed before construction; preserves the byte stream and decoder behavior.
use kio_core::{KioError, Result};

pub(super) struct MetadataBudget {
    pub(super) header_peak: u64,
    pub(super) scan_peak: u64,
}

pub(super) fn metadata_budget(bytes: &[u8]) -> Result<MetadataBudget> {
    let mut state = State::default();
    let mut pos = 2;
    let mut scanning = false;
    let mut header_peak = 0;
    let mut scan_peak = 0;
    while pos < bytes.len() {
        // Scan entropy without decoding. FF00 is a stuffed literal; restart
        // markers have no segment length. Other markers leave the scan.
        if bytes[pos] != 255 {
            pos += 1;
            continue;
        }
        pos += 1;
        while bytes.get(pos) == Some(&255) {
            pos += 1;
        }
        let Some(&marker) = bytes.get(pos) else {
            break;
        };
        pos += 1;
        if marker == 0 || marker == 1 || (208..=215).contains(&marker) {
            continue;
        }
        if marker == 217 {
            break;
        }
        if marker == 216 {
            continue;
        }
        let h = bytes.get(pos..pos + 2).ok_or_else(invalid)?;
        let length = usize::from(u16::from_be_bytes([h[0], h[1]]));
        if length < 2 {
            return Err(invalid());
        }
        let end = pos.checked_add(length).ok_or_else(invalid)?;
        let payload = bytes.get(pos + 2..end).ok_or_else(invalid)?;
        pos = end;
        state.marker(marker, payload)?;
        if !scanning {
            state.reassemble();
        }
        if scanning {
            scan_peak = scan_peak.max(state.peak);
        } else {
            header_peak = header_peak.max(state.peak);
        }
        if marker == 218 && !scanning {
            scanning = true;
            // Only live metadata overlaps later MCU arrays. Header-only
            // transient concat/replace/sort peaks overlap caller output, but
            // precede coefficient/upsample allocation.
            scan_peak = state.live;
            state.peak = state.live;
        }
    }
    Ok(MetadataBudget {
        header_peak,
        scan_peak: scan_peak.max(state.live),
    })
}
fn invalid() -> KioError {
    KioError::schema("JPEG metadata allocation preflight rejected invalid layout")
}

#[derive(Default)]
struct RecordVec {
    count: u64,
    capacity: u64,
}
impl RecordVec {
    fn push(&mut self, live: &mut u64, peak: &mut u64, record: u64) {
        self.count += 1;
        if self.count > self.capacity {
            let capacity = (2 * self.capacity).max(4);
            // realloc can request the new allocation while the old one lives.
            *peak = (*peak).max(*live + capacity * record);
            *live += (capacity - self.capacity) * record;
            self.capacity = capacity;
        }
    }
}

#[derive(Clone)]
struct Segment<'a> {
    offset: u32,
    total: u32,
    guid: &'a [u8],
    length: u64,
    order: usize,
}
#[derive(Default)]
struct State<'a> {
    live: u64,
    peak: u64,
    single: [u64; 6], // EXIF, standard XMP, IPTC, MPF, extended XMP, GUID
    icc: RecordVec,
    gain: RecordVec,
    extended: RecordVec,
    segments: Vec<Segment<'a>>,
    segment_payload: u64,
    segment_data: u64,
    order: usize,
    components: bool,
}
const VEC: u64 = 3 * std::mem::size_of::<usize>() as u64;
const WORD: u64 = std::mem::size_of::<usize>() as u64;
const ICC_RECORD: u64 = (VEC + 2).div_ceil(WORD) * WORD;
const EXT_RECORD: u64 = 2 * VEC + 8;
// Components' five Vecs; eleven pointer/usize fields; SampleRatios
// discriminant + two usize payload; [i32;64], dc_pred, four byte fields.
// Locked Rust1.98 layout:496B on supported64-bit targets.
const COMPONENT_RECORD: u64 = 5 * VEC + 14 * WORD + 264;
impl<'a> State<'a> {
    fn allocate(&mut self, bytes: u64) {
        self.live += bytes;
        self.peak = self.peak.max(self.live);
    }
    fn replace(&mut self, slot: usize, bytes: usize) {
        let bytes = bytes as u64;
        self.allocate(bytes); // RHS is allocated before old Option is dropped.
        self.live -= self.single[slot];
        self.single[slot] = bytes;
    }
    fn marker(&mut self, marker: u8, p: &'a [u8]) -> Result<()> {
        const XMP: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
        const EXT: &[u8] = b"http://ns.adobe.com/xmp/extension/\0";
        const HDR: &[u8] = b"urn:iso:std:iso:ts:21496:-1\0";
        if matches!(marker, 192..=194) && !self.components {
            if p.len() < 6 {
                return Err(invalid());
            }
            self.allocate(u64::from(p[5]) * COMPONENT_RECORD);
            self.components = true;
        } else if marker == 225 {
            if p.len() > 6 && p.starts_with(b"Exif\0\0") {
                self.replace(0, p.len() - 6);
            } else if p.len() > XMP.len() && p.starts_with(XMP) {
                self.replace(1, p.len() - XMP.len());
            } else if p.len() > EXT.len() && p.starts_with(EXT) {
                let h = &p[EXT.len()..];
                if h.len() < 40 {
                    return Err(invalid());
                }
                let length = (h.len() - 40) as u64;
                self.allocate(32 + length);
                self.extended
                    .push(&mut self.live, &mut self.peak, EXT_RECORD);
                self.segments.push(Segment {
                    offset: u32::from_be_bytes(h[36..40].try_into().unwrap()),
                    total: u32::from_be_bytes(h[32..36].try_into().unwrap()),
                    guid: &h[..32],
                    length,
                    order: self.order,
                });
                self.order += 1;
                self.segment_payload += 32 + length;
                self.segment_data += length;
            }
        } else if marker == 226 {
            if p.len() > 14 && p.starts_with(b"ICC_PROFILE\0") {
                self.allocate((p.len() - 14) as u64);
                self.icc.push(&mut self.live, &mut self.peak, ICC_RECORD);
            } else if p.len() > HDR.len() && p.starts_with(HDR) {
                let length = p.len() - HDR.len();
                if length >= 4 {
                    if length > 4 {
                        self.allocate(length as u64);
                    }
                    self.gain.push(&mut self.live, &mut self.peak, VEC);
                }
            } else if p.len() > 4 && p.starts_with(b"MPF\0") {
                self.replace(3, p.len() - 4);
            }
        } else if marker == 237 && p.len() > 14 && p.starts_with(b"Photoshop 3.0\0") {
            self.replace(2, p.len() - 14);
        }
        Ok(())
    }
    fn clear_segments(&mut self) {
        self.live -= self.segment_payload;
        self.segment_payload = 0;
        self.segment_data = 0;
        self.extended.count = 0;
        self.segments.clear();
        // Both Vec capacities remain live after clear.
    }
    fn reassemble(&mut self) {
        if self.segments.is_empty() {
            return;
        }
        // Rust1.98 driftsort scratch max(ceil(n/2),min(n,8MB/T),48),
        // with 4KiB stack scratch; optimize_for_size uses no larger heap.
        let n = self.extended.count;
        let scratch_records = (n - n / 2).max(n.min(8_000_000 / EXT_RECORD)).max(48);
        let sort_scratch = if n <= 20 || scratch_records * EXT_RECORD <= 4096 {
            0
        } else {
            scratch_records * EXT_RECORD
        };
        self.peak = self.peak.max(self.live + sort_scratch);
        let first = &self.segments[0];
        if self
            .segments
            .iter()
            .any(|s| s.guid != first.guid || s.total != first.total)
        {
            self.clear_segments();
            return;
        }
        let total = first.total;
        // With less than total payload neither completion nor prefix overflow
        // can occur. Avoid sorting an ever-growing incomplete stream here;
        // the decoder's sort allocation above is still accounted each time.
        if self.segment_data < u64::from(total) {
            return;
        }
        self.segments.sort_unstable_by_key(|s| (s.offset, s.order));
        let mut rolling = 0_u64;
        let mut complete = true;
        for segment in &self.segments {
            if u64::from(segment.offset) != rolling {
                complete = false;
                break;
            }
            rolling += segment.length;
        }
        if complete && rolling == u64::from(total) {
            // Result copied while segment payloads remain. Then GUID cloned
            // while the previous GUID allocation still exists.
            self.allocate(u64::from(total));
            self.live -= self.single[4];
            self.single[4] = u64::from(total);
            self.replace(5, 32);
            self.clear_segments();
        } else if rolling > u64::from(total) {
            self.clear_segments();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app(marker: u8, p: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(&[255, marker]);
        out.extend_from_slice(&((p.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(p);
    }
    #[test]
    fn o2_jpeg_metadata_records_and_post_sos_markers_are_charged() {
        let mut bytes = vec![255, 216];
        for _ in 0..5 {
            app(226, b"ICC_PROFILE\0\x01\x01\x00", &mut bytes);
        }
        app(218, &[], &mut bytes);
        bytes.extend_from_slice(&[17, 255, 0, 226, 33, 255, 208]);
        app(226, b"ICC_PROFILE\0\x01\x01\x00", &mut bytes);
        bytes.extend_from_slice(&[255, 217]);
        let b = metadata_budget(&bytes).unwrap();
        assert_eq!(b.header_peak, 5 + (4 + 8) * ICC_RECORD);
        assert_eq!(b.scan_peak, 6 + 8 * ICC_RECORD);
    }
    #[test]
    fn o2_jpeg_replaced_metadata_is_not_retained_forever() {
        let mut bytes = vec![255, 216];
        app(225, b"Exif\0\0abc", &mut bytes);
        app(225, b"Exif\0\0de", &mut bytes);
        app(218, &[], &mut bytes);
        let b = metadata_budget(&bytes).unwrap();
        assert_eq!(b.header_peak, 5);
        assert_eq!(b.scan_peak, 2);
    }
    #[test]
    fn o2_jpeg_extended_xmp_concat_and_guid_clone_overlap() {
        let mut bytes = vec![255, 216];
        let mut xmp = b"http://ns.adobe.com/xmp/extension/\0".to_vec();
        xmp.extend_from_slice(&[b'A'; 32]);
        xmp.extend_from_slice(&3_u32.to_be_bytes());
        xmp.extend_from_slice(&0_u32.to_be_bytes());
        xmp.extend_from_slice(b"abc");
        app(225, &xmp, &mut bytes);
        app(218, &[], &mut bytes);
        let b = metadata_budget(&bytes).unwrap();
        assert_eq!(b.header_peak, 4 * EXT_RECORD + 35 + 3 + 32);
        assert_eq!(b.scan_peak, 4 * EXT_RECORD + 3 + 32);
    }
    fn extended(guid: u8, total: u32, offset: u32, data: &[u8], out: &mut Vec<u8>) {
        let mut p = b"http://ns.adobe.com/xmp/extension/\0".to_vec();
        p.extend_from_slice(&[guid; 32]);
        p.extend_from_slice(&total.to_be_bytes());
        p.extend_from_slice(&offset.to_be_bytes());
        p.extend_from_slice(data);
        app(225, &p, out);
    }
    #[test]
    fn o2_jpeg_xmp_missing_parts_stay_retained_and_inconsistency_clears() {
        let mut missing = vec![255, 216];
        extended(b'A', 6, 3, b"def", &mut missing);
        app(218, &[], &mut missing);
        let b = metadata_budget(&missing).unwrap();
        assert_eq!(b.scan_peak, 4 * EXT_RECORD + 35);
        let mut inconsistent = vec![255, 216];
        extended(b'A', 6, 0, b"abc", &mut inconsistent);
        extended(b'B', 6, 3, b"def", &mut inconsistent);
        app(218, &[], &mut inconsistent);
        let b = metadata_budget(&inconsistent).unwrap();
        assert_eq!(b.header_peak, 4 * EXT_RECORD + 70);
        assert_eq!(b.scan_peak, 4 * EXT_RECORD);
        let mut overflow = vec![255, 216];
        extended(b'A', 2, 0, b"abc", &mut overflow);
        app(218, &[], &mut overflow);
        assert_eq!(
            metadata_budget(&overflow).unwrap().scan_peak,
            4 * EXT_RECORD
        );
    }
    #[test]
    fn o2_jpeg_xmp_out_of_order_complete_parts_reassemble() {
        let mut bytes = vec![255, 216];
        extended(b'A', 6, 3, b"def", &mut bytes);
        extended(b'A', 6, 0, b"abc", &mut bytes);
        app(218, &[], &mut bytes);
        let b = metadata_budget(&bytes).unwrap();
        assert_eq!(b.header_peak, 4 * EXT_RECORD + 70 + 6 + 32);
        assert_eq!(b.scan_peak, 4 * EXT_RECORD + 6 + 32);
    }
    #[test]
    fn o2_jpeg_gain_records_and_mpf_iptc_replacements_are_charged() {
        let mut bytes = vec![255, 216];
        let prefix = b"urn:iso:std:iso:ts:21496:-1\0";
        for payload in [&[0_u8; 4][..], &[0_u8; 5][..]] {
            let mut p = prefix.to_vec();
            p.extend_from_slice(payload);
            app(226, &p, &mut bytes);
        }
        app(226, b"MPF\0ab", &mut bytes);
        app(226, b"MPF\0cde", &mut bytes);
        app(237, b"Photoshop 3.0\0abc", &mut bytes);
        app(237, b"Photoshop 3.0\0de", &mut bytes);
        app(218, &[], &mut bytes);
        let b = metadata_budget(&bytes).unwrap();
        assert_eq!(b.header_peak, 4 * VEC + 5 + 3 + 3 + 2);
        assert_eq!(b.scan_peak, 4 * VEC + 5 + 3 + 2);
    }
    #[test]
    fn o2_jpeg_post_sos_xmp_is_retained_without_header_reassembly() {
        let mut bytes = vec![255, 216];
        app(218, &[], &mut bytes);
        extended(b'A', 3, 0, b"abc", &mut bytes);
        bytes.extend_from_slice(&[255, 217]);
        let b = metadata_budget(&bytes).unwrap();
        assert_eq!(b.header_peak, 0);
        assert_eq!(b.scan_peak, 4 * EXT_RECORD + 35);
    }
}
