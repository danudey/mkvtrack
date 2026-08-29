//! EBML primitives: variable length integers, element headers, and a small
//! in-memory element tree that round-trips losslessly.

/// Marker for an element whose size is coded as "unknown" (all value bits set).
pub const UNKNOWN: u64 = u64::MAX;

/// Element IDs used by this tool. Stored in canonical form, i.e. with the
/// VINT marker bits included, the way they appear on disk.
#[allow(dead_code)]
pub mod id {
    pub const EBML_HEAD: u64 = 0x1A45_DFA3;
    pub const SEGMENT: u64 = 0x1853_8067;

    pub const SEEK_HEAD: u64 = 0x114D_9B74;
    pub const SEEK: u64 = 0x4DBB;
    pub const SEEK_ID: u64 = 0x53AB;
    pub const SEEK_POSITION: u64 = 0x53AC;

    pub const INFO: u64 = 0x1549_A966;
    pub const TIMESTAMP_SCALE: u64 = 0x2AD7B1;
    pub const DURATION: u64 = 0x4489;
    pub const TITLE: u64 = 0x7BA9;
    pub const MUXING_APP: u64 = 0x4D80;
    pub const WRITING_APP: u64 = 0x5741;

    pub const TRACKS: u64 = 0x1654_AE6B;
    pub const TRACK_ENTRY: u64 = 0xAE;
    pub const TRACK_NUMBER: u64 = 0xD7;
    pub const TRACK_UID: u64 = 0x73C5;
    pub const TRACK_TYPE: u64 = 0x83;
    pub const FLAG_ENABLED: u64 = 0xB9;
    pub const FLAG_DEFAULT: u64 = 0x88;
    pub const FLAG_FORCED: u64 = 0x55AA;
    pub const FLAG_HEARING_IMPAIRED: u64 = 0x55AB;
    pub const FLAG_VISUAL_IMPAIRED: u64 = 0x55AC;
    pub const FLAG_TEXT_DESCRIPTIONS: u64 = 0x55AD;
    pub const FLAG_ORIGINAL: u64 = 0x55AE;
    pub const FLAG_COMMENTARY: u64 = 0x55AF;
    pub const FLAG_LACING: u64 = 0x9C;
    pub const DEFAULT_DURATION: u64 = 0x23E383;
    pub const TRACK_NAME: u64 = 0x536E;
    pub const LANGUAGE: u64 = 0x22B59C;
    pub const LANGUAGE_BCP47: u64 = 0x22B59D;
    pub const CODEC_ID: u64 = 0x86;
    pub const CODEC_PRIVATE: u64 = 0x63A2;
    pub const CODEC_NAME: u64 = 0x258688;
    pub const CODEC_DELAY: u64 = 0x56AA;
    pub const SEEK_PRE_ROLL: u64 = 0x56BB;
    pub const CONTENT_ENCODINGS: u64 = 0x6D80;

    pub const VIDEO: u64 = 0xE0;
    pub const PIXEL_WIDTH: u64 = 0xB0;
    pub const PIXEL_HEIGHT: u64 = 0xBA;
    pub const DISPLAY_WIDTH: u64 = 0x54B0;
    pub const DISPLAY_HEIGHT: u64 = 0x54BA;

    pub const AUDIO: u64 = 0xE1;
    pub const SAMPLING_FREQUENCY: u64 = 0xB5;
    pub const OUTPUT_SAMPLING_FREQUENCY: u64 = 0x78B5;
    pub const CHANNELS: u64 = 0x9F;
    pub const BIT_DEPTH: u64 = 0x6264;

    pub const CLUSTER: u64 = 0x1F43_B675;
    pub const CLUSTER_TIMESTAMP: u64 = 0xE7;
    pub const CLUSTER_POSITION: u64 = 0xA7;
    pub const SIMPLE_BLOCK: u64 = 0xA3;
    pub const BLOCK_GROUP: u64 = 0xA0;

    pub const CUES: u64 = 0x1C53_BB6B;
    pub const CUE_POINT: u64 = 0xBB;
    pub const CUE_TRACK_POSITIONS: u64 = 0xB7;
    pub const CUE_CLUSTER_POSITION: u64 = 0xF1;
    pub const CUE_REFERENCE: u64 = 0xDB;

    pub const CHAPTERS: u64 = 0x1043_A770;
    pub const TAGS: u64 = 0x1254_C367;
    pub const ATTACHMENTS: u64 = 0x1941_A469;

    pub const VOID: u64 = 0xEC;
    pub const CRC32: u64 = 0xBF;
}

/// Track types as defined by the Matroska specification.
#[allow(dead_code)]
pub mod track_type {
    pub const VIDEO: u64 = 1;
    pub const AUDIO: u64 = 2;
    pub const COMPLEX: u64 = 3;
    pub const LOGO: u64 = 0x10;
    pub const SUBTITLE: u64 = 0x11;
    pub const BUTTONS: u64 = 0x12;
    pub const CONTROL: u64 = 0x20;
    pub const METADATA: u64 = 0x21;
}

pub fn track_type_name(t: u64) -> &'static str {
    match t {
        track_type::VIDEO => "video",
        track_type::AUDIO => "audio",
        track_type::COMPLEX => "complex",
        track_type::LOGO => "logo",
        track_type::SUBTITLE => "subtitle",
        track_type::BUTTONS => "buttons",
        track_type::CONTROL => "control",
        track_type::METADATA => "metadata",
        _ => "unknown",
    }
}

/// Number of bytes an element ID occupies, from its first byte.
pub fn id_len_from_first(first: u8) -> Option<usize> {
    for i in 0..4 {
        if first & (0x80 >> i) != 0 {
            return Some(i + 1);
        }
    }
    None
}

/// Number of bytes a size VINT occupies, from its first byte.
pub fn vint_len_from_first(first: u8) -> Option<usize> {
    if first == 0 {
        None
    } else {
        Some(first.leading_zeros() as usize + 1)
    }
}

pub fn read_id(buf: &[u8], pos: usize) -> Option<(u64, usize)> {
    let first = *buf.get(pos)?;
    let len = id_len_from_first(first)?;
    if pos + len > buf.len() {
        return None;
    }
    let mut v = 0u64;
    for i in 0..len {
        v = (v << 8) | buf[pos + i] as u64;
    }
    Some((v, len))
}

/// Reads a data-size VINT. Returns [`UNKNOWN`] when every value bit is set.
pub fn read_size(buf: &[u8], pos: usize) -> Option<(u64, usize)> {
    let first = *buf.get(pos)?;
    let len = vint_len_from_first(first)?;
    if len > 8 || pos + len > buf.len() {
        return None;
    }
    let mask: u8 = if len >= 8 { 0 } else { 0xFF >> len };
    let mut v = (first & mask) as u64;
    let mut all_ones = (first & mask) == mask;
    for i in 1..len {
        let b = buf[pos + i];
        v = (v << 8) | b as u64;
        all_ones &= b == 0xFF;
    }
    if all_ones {
        Some((UNKNOWN, len))
    } else {
        Some((v, len))
    }
}

/// Bytes needed to store an element ID.
pub fn id_len(id: u64) -> usize {
    if id <= 0xFF {
        1
    } else if id <= 0xFFFF {
        2
    } else if id <= 0x00FF_FFFF {
        3
    } else {
        4
    }
}

pub fn write_id(out: &mut Vec<u8>, id: u64) {
    let len = id_len(id);
    for i in (0..len).rev() {
        out.push(((id >> (8 * i)) & 0xFF) as u8);
    }
}

/// Smallest VINT width that can hold `v` without colliding with the
/// "unknown size" encoding.
pub fn size_len(v: u64) -> usize {
    for len in 1..8usize {
        if v <= (1u64 << (7 * len)) - 2 {
            return len;
        }
    }
    8
}

pub fn write_size_with_len(out: &mut Vec<u8>, v: u64, len: usize) {
    debug_assert!(
        len >= size_len(v) && len <= 8,
        "value {v} does not fit in {len} bytes"
    );
    let mut bytes = [0u8; 8];
    for i in 0..len {
        bytes[len - 1 - i] = ((v >> (8 * i)) & 0xFF) as u8;
    }
    bytes[0] |= 0x80 >> (len - 1);
    out.extend_from_slice(&bytes[..len]);
}

pub fn write_size(out: &mut Vec<u8>, v: u64) {
    write_size_with_len(out, v, size_len(v));
}

pub fn read_uint(data: &[u8]) -> u64 {
    let mut v = 0u64;
    for &b in data.iter().take(8) {
        v = (v << 8) | b as u64;
    }
    v
}

pub fn uint_bytes(v: u64) -> Vec<u8> {
    if v == 0 {
        return vec![0];
    }
    let n = 8 - (v.leading_zeros() / 8) as usize;
    (0..n)
        .rev()
        .map(|i| ((v >> (8 * i)) & 0xFF) as u8)
        .collect()
}

/// Encodes `v` in exactly `len` bytes, or `None` when it does not fit.
pub fn uint_bytes_fixed(v: u64, len: usize) -> Option<Vec<u8>> {
    let min = uint_bytes(v);
    if min.len() > len {
        return None;
    }
    let mut out = vec![0u8; len - min.len()];
    out.extend_from_slice(&min);
    Some(out)
}

pub fn read_float(data: &[u8]) -> f64 {
    match data.len() {
        4 => f32::from_be_bytes([data[0], data[1], data[2], data[3]]) as f64,
        8 => f64::from_be_bytes([
            data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
        ]),
        _ => 0.0,
    }
}

pub fn read_string(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).into_owned()
}

/// CRC-32 as used by Matroska: IEEE 802.3, stored little endian.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Builds a Void element occupying exactly `total` bytes. `total` must be at
/// least 2 (an ID byte plus a size byte).
pub fn void_of(total: usize) -> Vec<u8> {
    assert!(total >= 2, "a Void element needs at least 2 bytes");
    let mut out = Vec::with_capacity(total);
    out.push(id::VOID as u8);
    if total <= 2 + 0x7E {
        write_size_with_len(&mut out, (total - 2) as u64, 1);
        out.resize(total, 0);
    } else {
        // A fixed 8 byte size VINT keeps the arithmetic simple for big gaps.
        write_size_with_len(&mut out, (total - 9) as u64, 8);
        out.resize(total, 0);
    }
    out
}

// ---------------------------------------------------------------------------
// Element tree
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum Body {
    Master(Vec<Element>),
    Data(Vec<u8>),
}

#[derive(Clone, Debug)]
pub struct Element {
    pub id: u64,
    pub body: Body,
}

impl Element {
    pub fn master(id: u64, children: Vec<Element>) -> Self {
        Element {
            id,
            body: Body::Master(children),
        }
    }

    pub fn data(id: u64, data: Vec<u8>) -> Self {
        Element {
            id,
            body: Body::Data(data),
        }
    }

    pub fn uint_elem(id: u64, v: u64) -> Self {
        Element::data(id, uint_bytes(v))
    }

    pub fn string_elem(id: u64, s: &str) -> Self {
        Element::data(id, s.as_bytes().to_vec())
    }

    pub fn children(&self) -> &[Element] {
        match &self.body {
            Body::Master(c) => c,
            Body::Data(_) => &[],
        }
    }

    pub fn children_mut(&mut self) -> Option<&mut Vec<Element>> {
        match &mut self.body {
            Body::Master(c) => Some(c),
            Body::Data(_) => None,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        match &self.body {
            Body::Data(d) => d,
            Body::Master(_) => &[],
        }
    }

    pub fn find(&self, id: u64) -> Option<&Element> {
        self.children().iter().find(|c| c.id == id)
    }

    pub fn find_mut(&mut self, id: u64) -> Option<&mut Element> {
        self.children_mut()?.iter_mut().find(|c| c.id == id)
    }

    pub fn get_uint(&self, id: u64) -> Option<u64> {
        self.find(id).map(|e| read_uint(e.bytes()))
    }

    pub fn get_float(&self, id: u64) -> Option<f64> {
        self.find(id).map(|e| read_float(e.bytes()))
    }

    pub fn get_string(&self, id: u64) -> Option<String> {
        self.find(id).map(|e| read_string(e.bytes()))
    }

    /// Replaces the value of `id`, or appends the element at `insert_at` when
    /// it is missing. Returns true when the element already existed.
    pub fn set_uint(&mut self, id: u64, v: u64, insert_at: usize) -> bool {
        if let Some(e) = self.find_mut(id) {
            e.body = Body::Data(uint_bytes(v));
            true
        } else {
            let children = match self.children_mut() {
                Some(c) => c,
                None => return false,
            };
            let at = insert_at.min(children.len());
            children.insert(at, Element::uint_elem(id, v));
            false
        }
    }

    pub fn set_string(&mut self, id: u64, s: &str, insert_at: usize) {
        if let Some(e) = self.find_mut(id) {
            e.body = Body::Data(s.as_bytes().to_vec());
        } else if let Some(children) = self.children_mut() {
            let at = insert_at.min(children.len());
            children.insert(at, Element::string_elem(id, s));
        }
    }

    pub fn remove(&mut self, id: u64) -> bool {
        if let Some(children) = self.children_mut() {
            let before = children.len();
            children.retain(|c| c.id != id);
            children.len() != before
        } else {
            false
        }
    }

    /// Serializes the element, recomputing CRC-32 where one was present and
    /// dropping Void padding.
    pub fn serialize(&self, out: &mut Vec<u8>) {
        let payload: Vec<u8> = match &self.body {
            Body::Data(d) => d.clone(),
            Body::Master(children) => {
                let has_crc = children.iter().any(|c| c.id == id::CRC32);
                let mut body = Vec::new();
                for c in children {
                    if c.id == id::CRC32 || c.id == id::VOID {
                        continue;
                    }
                    c.serialize(&mut body);
                }
                if has_crc {
                    let mut with_crc = Vec::with_capacity(body.len() + 6);
                    with_crc.push(id::CRC32 as u8);
                    with_crc.push(0x84);
                    with_crc.extend_from_slice(&crc32(&body).to_le_bytes());
                    with_crc.extend_from_slice(&body);
                    with_crc
                } else {
                    body
                }
            }
        };
        write_id(out, self.id);
        write_size(out, payload.len() as u64);
        out.extend_from_slice(&payload);
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.serialize(&mut out);
        out
    }
}

/// IDs that hold child elements. Anything else is kept as an opaque blob so
/// that unknown or unhandled elements survive a rewrite untouched.
pub fn is_master(id: u64) -> bool {
    matches!(
        id,
        id::SEGMENT
            | id::SEEK_HEAD
            | id::SEEK
            | id::INFO
            | id::TRACKS
            | id::TRACK_ENTRY
            | id::VIDEO
            | id::AUDIO
            | id::CONTENT_ENCODINGS
            | id::CUES
            | id::CUE_POINT
            | id::CUE_TRACK_POSITIONS
            | id::CUE_REFERENCE
    )
}

/// Parses the children of a master element from its payload.
pub fn parse_children(buf: &[u8]) -> Result<Vec<Element>, String> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < buf.len() {
        let (id, id_len) = read_id(buf, pos).ok_or("bad element ID")?;
        let (size, size_len) = read_size(buf, pos + id_len).ok_or("bad element size")?;
        let start = pos + id_len + size_len;
        if size == UNKNOWN {
            return Err("unknown size inside a master element".into());
        }
        let end = start
            .checked_add(size as usize)
            .filter(|e| *e <= buf.len())
            .ok_or("element runs past the end of its parent")?;
        let payload = &buf[start..end];
        let el = if is_master(id) {
            Element::master(id, parse_children(payload)?)
        } else {
            Element::data(id, payload.to_vec())
        };
        out.push(el);
        pos = end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vints_round_trip() {
        for v in [0u64, 1, 126, 127, 128, 16382, 16383, 1 << 20, (1 << 56) - 3] {
            let mut buf = Vec::new();
            write_size(&mut buf, v);
            let (got, len) = read_size(&buf, 0).unwrap();
            assert_eq!(got, v, "value {v}");
            assert_eq!(len, buf.len());
        }
    }

    #[test]
    fn padded_vints_decode_to_the_same_value() {
        for len in size_len(300)..=8usize {
            let mut buf = Vec::new();
            write_size_with_len(&mut buf, 300, len);
            assert_eq!(buf.len(), len);
            assert_eq!(read_size(&buf, 0).unwrap().0, 300);
        }
    }

    #[test]
    fn unknown_size_is_recognised() {
        let buf = [0xFFu8];
        assert_eq!(read_size(&buf, 0).unwrap().0, UNKNOWN);
        let buf = [0x01u8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(read_size(&buf, 0).unwrap().0, UNKNOWN);
    }

    #[test]
    fn ids_round_trip() {
        for id in [id::VOID, id::FLAG_FORCED, id::LANGUAGE, id::SEGMENT] {
            let mut buf = Vec::new();
            write_id(&mut buf, id);
            assert_eq!(buf.len(), id_len(id));
            assert_eq!(read_id(&buf, 0).unwrap(), (id, id_len(id)));
        }
    }

    #[test]
    fn void_has_the_requested_length() {
        for total in [2usize, 3, 10, 128, 129, 5000] {
            let v = void_of(total);
            assert_eq!(v.len(), total);
            let (id, il) = read_id(&v, 0).unwrap();
            let (size, sl) = read_size(&v, il).unwrap();
            assert_eq!(id, id::VOID);
            assert_eq!(il + sl + size as usize, total);
        }
    }

    #[test]
    fn crc32_matches_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn tree_round_trips() {
        let src = Element::master(
            id::TRACKS,
            vec![Element::master(
                id::TRACK_ENTRY,
                vec![
                    Element::uint_elem(id::TRACK_NUMBER, 1),
                    Element::uint_elem(id::TRACK_TYPE, 2),
                    Element::string_elem(id::CODEC_ID, "A_AAC"),
                ],
            )],
        );
        let bytes = src.to_bytes();
        let (id, il) = read_id(&bytes, 0).unwrap();
        let (size, sl) = read_size(&bytes, il).unwrap();
        assert_eq!(id, id::TRACKS);
        let parsed = parse_children(&bytes[il + sl..il + sl + size as usize]).unwrap();
        let round = Element::master(id::TRACKS, parsed);
        assert_eq!(round.to_bytes(), bytes);
    }
}
