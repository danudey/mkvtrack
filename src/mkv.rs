//! Matroska file model: locate the Segment, its top level children and the
//! Tracks element, and present track entries in a form the UI can render.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::ebml::{self, Element, UNKNOWN, id};

/// A direct child of the Segment element, located on disk.
#[derive(Clone, Debug)]
pub struct TopChild {
    pub id: u64,
    /// Absolute offset of the element ID.
    pub start: u64,
    /// Bytes of ID plus size VINT.
    pub header_len: u64,
    /// Payload length.
    pub size: u64,
}

impl TopChild {
    pub fn data_start(&self) -> u64 {
        self.start + self.header_len
    }
    pub fn end(&self) -> u64 {
        self.start + self.header_len + self.size
    }
    pub fn total_len(&self) -> u64 {
        self.header_len + self.size
    }
}

#[derive(Clone, Debug, Default)]
pub struct FileInfo {
    pub title: Option<String>,
    pub duration_secs: Option<f64>,
    pub muxing_app: Option<String>,
    pub writing_app: Option<String>,
}

/// How far the walk of the Segment's children got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scan {
    /// Every child was located.
    Complete,
    /// The walk stopped at the first Cluster, having already found Tracks.
    /// There are more children after it; see [`MkvFile::scan_all`].
    StoppedAtClusters,
    /// The walk could not go any further, and says why.
    Blocked(String),
}

#[derive(Clone, Debug)]
pub struct MkvFile {
    pub path: PathBuf,
    pub file_len: u64,
    /// Modification time when the file was read, used to notice a file that
    /// changed underneath us before a write.
    pub modified: Option<SystemTime>,
    pub segment_start: u64,
    pub segment_data_start: u64,
    /// Where the Segment payload ends on disk.
    pub segment_end: u64,
    /// Children of the Segment up to and including the first Cluster. Opening
    /// a file stops there: reading a track list needs nothing beyond it, and a
    /// full walk of a long film costs one seek per cluster.
    pub top: Vec<TopChild>,
    pub scan: Scan,
    /// Index into `top` of the Tracks element.
    pub tracks_idx: usize,
    /// The parsed Tracks element. This is the editable model.
    pub tracks: Element,
    pub info: FileInfo,
}

pub(crate) fn read_at(f: &mut File, pos: u64, len: usize) -> std::io::Result<Vec<u8>> {
    f.seek(SeekFrom::Start(pos))?;
    let mut buf = vec![0u8; len];
    let mut got = 0usize;
    while got < len {
        match f.read(&mut buf[got..])? {
            0 => break,
            n => got += n,
        }
    }
    buf.truncate(got);
    Ok(buf)
}

/// Walks the direct children of the Segment from `from` to `end`.
///
/// With `stop_at_clusters` the walk ends at the first Cluster once Tracks has
/// been seen. That is everything the track list needs, and it keeps opening a
/// file down to a handful of reads however long the film is; the full walk
/// costs one seek per cluster and is only needed to rewrite the file.
fn walk_segment(
    f: &mut File,
    from: u64,
    end: u64,
    stop_at_clusters: bool,
) -> (Vec<TopChild>, Scan) {
    let mut top: Vec<TopChild> = Vec::new();
    let mut pos = from;
    let mut seen_tracks = false;
    while pos < end {
        let (eid, size, hlen) = match read_header(f, pos) {
            Ok(v) => v,
            Err(e) => return (top, Scan::Blocked(e)),
        };
        if size == UNKNOWN {
            // Streamed clusters have no length. Editing in place still works,
            // but the file cannot be rewritten.
            return (
                top,
                Scan::Blocked(format!("element {eid:#X} at {pos} has an unknown size")),
            );
        }
        let child = TopChild {
            id: eid,
            start: pos,
            header_len: hlen,
            size,
        };
        if child.end() > end {
            return (
                top,
                Scan::Blocked(format!("element {eid:#X} at {pos} runs past the Segment")),
            );
        }
        pos = child.end();
        top.push(child);
        seen_tracks |= eid == id::TRACKS;
        if stop_at_clusters && seen_tracks && eid == id::CLUSTER {
            return (top, Scan::StoppedAtClusters);
        }
    }
    (top, Scan::Complete)
}

/// Reads an element header at `pos`. Returns (id, size, header_len).
pub(crate) fn read_header(f: &mut File, pos: u64) -> Result<(u64, u64, u64), String> {
    let buf = read_at(f, pos, 12).map_err(|e| e.to_string())?;
    let (id, il) = ebml::read_id(&buf, 0).ok_or_else(|| format!("bad element ID at {pos}"))?;
    let (size, sl) =
        ebml::read_size(&buf, il).ok_or_else(|| format!("bad element size at {pos}"))?;
    Ok((id, size, (il + sl) as u64))
}

impl MkvFile {
    pub fn open(path: &Path) -> Result<MkvFile, String> {
        let mut f = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let meta = f.metadata().map_err(|e| e.to_string())?;
        let file_len = meta.len();
        let modified = meta.modified().ok();

        // EBML header, then Segment. Skip any leading Void.
        let mut pos = 0u64;
        let mut segment: Option<(u64, u64, u64)> = None;
        while pos + 4 < file_len {
            let (id, size, hlen) = read_header(&mut f, pos)?;
            match id {
                id::EBML_HEAD | id::VOID => {
                    if size == UNKNOWN {
                        return Err("EBML header has an unknown size".into());
                    }
                    pos += hlen + size;
                }
                id::SEGMENT => {
                    segment = Some((pos, size, hlen));
                    break;
                }
                other => return Err(format!("not a Matroska file (top level ID {other:#X})")),
            }
        }
        let (segment_start, segment_size, segment_hlen) =
            segment.ok_or("no Segment element found")?;
        let segment_data_start = segment_start + segment_hlen;
        let segment_end = if segment_size == UNKNOWN {
            file_len
        } else {
            (segment_data_start + segment_size).min(file_len)
        };

        // Walk the Segment's direct children, stopping at the clusters. The
        // walk only stops once Tracks has been seen, so a file that puts
        // Tracks after the clusters, which is unusual but legal, still gets
        // the full walk it needs.
        let (top, scan) = walk_segment(&mut f, segment_data_start, segment_end, true);

        let tracks_idx = top
            .iter()
            .position(|c| c.id == id::TRACKS)
            .ok_or("no Tracks element found")?;
        let tc = top[tracks_idx].clone();
        if tc.size > 64 * 1024 * 1024 {
            return Err("Tracks element is implausibly large".into());
        }
        let payload =
            read_at(&mut f, tc.data_start(), tc.size as usize).map_err(|e| e.to_string())?;
        let tracks = Element::master(
            id::TRACKS,
            ebml::parse_children(&payload).map_err(|e| format!("Tracks: {e}"))?,
        );

        let mut info = FileInfo::default();
        let mut timestamp_scale = 1_000_000f64;
        if let Some(ic) = top.iter().find(|c| c.id == id::INFO)
            && ic.size < 1024 * 1024
            && let Ok(buf) = read_at(&mut f, ic.data_start(), ic.size as usize)
            && let Ok(children) = ebml::parse_children(&buf)
        {
            let el = Element::master(id::INFO, children);
            if let Some(ts) = el.get_uint(id::TIMESTAMP_SCALE)
                && ts > 0
            {
                timestamp_scale = ts as f64;
            }
            info.title = el.get_string(id::TITLE).filter(|s| !s.is_empty());
            info.muxing_app = el.get_string(id::MUXING_APP).filter(|s| !s.is_empty());
            info.writing_app = el.get_string(id::WRITING_APP).filter(|s| !s.is_empty());
            info.duration_secs = el
                .get_float(id::DURATION)
                .map(|d| d * timestamp_scale / 1_000_000_000.0)
                .filter(|d| *d > 0.0);
        }

        Ok(MkvFile {
            path: path.to_path_buf(),
            file_len,
            modified,
            segment_start,
            segment_data_start,
            segment_end,
            top,
            scan,
            tracks_idx,
            tracks,
            info,
        })
    }

    /// Every child of the Segment, walking past the clusters this time. Only
    /// the rewrite path needs this, so the cost is paid once, on the one file
    /// being written, rather than on every file the cursor passes over.
    pub fn scan_all(&self) -> Result<Vec<TopChild>, String> {
        if self.scan == Scan::Complete {
            return Ok(self.top.clone());
        }
        let mut f = File::open(&self.path).map_err(|e| format!("{}: {e}", self.path.display()))?;
        let (top, scan) = walk_segment(&mut f, self.segment_data_start, self.segment_end, false);
        match scan {
            Scan::Complete => Ok(top),
            Scan::Blocked(why) => Err(why),
            Scan::StoppedAtClusters => unreachable!("the full walk does not stop at clusters"),
        }
    }

    pub fn tracks_view(&self) -> Vec<Track> {
        self.tracks
            .children()
            .iter()
            .enumerate()
            .filter(|(_, e)| e.id == id::TRACK_ENTRY)
            .map(|(i, e)| Track::from_entry(i, e))
            .collect()
    }

    pub fn tracks_child(&self) -> &TopChild {
        &self.top[self.tracks_idx]
    }
}

// ---------------------------------------------------------------------------
// Track view model
// ---------------------------------------------------------------------------

/// A flag whose stored state we distinguish from its specification default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Flag {
    pub value: bool,
    /// True when the file carries the element explicitly.
    pub explicit: bool,
}

impl Flag {
    fn read(entry: &Element, id: u64, default: bool) -> Flag {
        match entry.get_uint(id) {
            Some(v) => Flag {
                value: v != 0,
                explicit: true,
            },
            None => Flag {
                value: default,
                explicit: false,
            },
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct AudioInfo {
    pub channels: Option<u64>,
    pub sampling_frequency: Option<f64>,
    pub output_sampling_frequency: Option<f64>,
    pub bit_depth: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct VideoInfo {
    pub pixel_width: Option<u64>,
    pub pixel_height: Option<u64>,
    pub display_width: Option<u64>,
    pub display_height: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Track {
    /// Index into `MkvFile::tracks.children()`.
    pub index: usize,
    pub number: u64,
    pub uid: u64,
    pub ttype: u64,
    pub codec_id: String,
    pub codec_name: Option<String>,
    pub name: Option<String>,
    pub language: String,
    pub language_explicit: bool,
    pub language_bcp47: Option<String>,
    pub default: Flag,
    pub forced: Flag,
    pub enabled: Flag,
    pub hearing_impaired: Flag,
    pub visual_impaired: Flag,
    pub text_descriptions: Flag,
    pub original: Flag,
    pub commentary: Flag,
    pub default_duration: Option<u64>,
    pub codec_delay: Option<u64>,
    pub codec_private_len: usize,
    pub compressed: bool,
    pub audio: Option<AudioInfo>,
    pub video: Option<VideoInfo>,
}

impl Track {
    pub fn from_entry(index: usize, e: &Element) -> Track {
        let audio = e.find(id::AUDIO).map(|a| AudioInfo {
            channels: a.get_uint(id::CHANNELS),
            sampling_frequency: a.get_float(id::SAMPLING_FREQUENCY),
            output_sampling_frequency: a.get_float(id::OUTPUT_SAMPLING_FREQUENCY),
            bit_depth: a.get_uint(id::BIT_DEPTH),
        });
        let video = e.find(id::VIDEO).map(|v| VideoInfo {
            pixel_width: v.get_uint(id::PIXEL_WIDTH),
            pixel_height: v.get_uint(id::PIXEL_HEIGHT),
            display_width: v.get_uint(id::DISPLAY_WIDTH),
            display_height: v.get_uint(id::DISPLAY_HEIGHT),
        });
        Track {
            index,
            number: e.get_uint(id::TRACK_NUMBER).unwrap_or(0),
            uid: e.get_uint(id::TRACK_UID).unwrap_or(0),
            ttype: e.get_uint(id::TRACK_TYPE).unwrap_or(0),
            codec_id: e.get_string(id::CODEC_ID).unwrap_or_default(),
            codec_name: e.get_string(id::CODEC_NAME).filter(|s| !s.is_empty()),
            name: e.get_string(id::TRACK_NAME).filter(|s| !s.is_empty()),
            language: e.get_string(id::LANGUAGE).unwrap_or_else(|| "eng".into()),
            language_explicit: e.find(id::LANGUAGE).is_some(),
            language_bcp47: e.get_string(id::LANGUAGE_BCP47).filter(|s| !s.is_empty()),
            default: Flag::read(e, id::FLAG_DEFAULT, true),
            forced: Flag::read(e, id::FLAG_FORCED, false),
            enabled: Flag::read(e, id::FLAG_ENABLED, true),
            hearing_impaired: Flag::read(e, id::FLAG_HEARING_IMPAIRED, false),
            visual_impaired: Flag::read(e, id::FLAG_VISUAL_IMPAIRED, false),
            text_descriptions: Flag::read(e, id::FLAG_TEXT_DESCRIPTIONS, false),
            original: Flag::read(e, id::FLAG_ORIGINAL, false),
            commentary: Flag::read(e, id::FLAG_COMMENTARY, false),
            default_duration: e.get_uint(id::DEFAULT_DURATION),
            codec_delay: e.get_uint(id::CODEC_DELAY),
            codec_private_len: e
                .find(id::CODEC_PRIVATE)
                .map(|c| c.bytes().len())
                .unwrap_or(0),
            compressed: e.find(id::CONTENT_ENCODINGS).is_some(),
            audio,
            video,
        }
    }

    pub fn type_name(&self) -> &'static str {
        ebml::track_type_name(self.ttype)
    }

    /// The language shown to the user: a BCP-47 tag takes precedence.
    pub fn effective_language(&self) -> String {
        self.language_bcp47
            .clone()
            .unwrap_or_else(|| self.language.clone())
    }

    pub fn display_name(&self) -> String {
        self.name.clone().unwrap_or_default()
    }

    /// Compact flag column, e.g. "D F HI". A flag the file states explicitly
    /// is upper case; one that comes from the specification default is lower
    /// case.
    pub fn flag_summary(&self) -> String {
        let mut out: Vec<&str> = Vec::new();
        if self.default.value {
            out.push(if self.default.explicit { "D" } else { "d" });
        }
        if self.forced.value {
            out.push(if self.forced.explicit { "F" } else { "f" });
        }
        if !self.enabled.value {
            out.push("off");
        }
        if self.hearing_impaired.value {
            out.push("HI");
        }
        if self.visual_impaired.value {
            out.push("VI");
        }
        if self.text_descriptions.value {
            out.push("TD");
        }
        if self.original.value {
            out.push("Orig");
        }
        if self.commentary.value {
            out.push("Com");
        }
        out.join(" ")
    }

    pub fn channel_layout(&self) -> String {
        match self.audio.as_ref().and_then(|a| a.channels) {
            Some(1) => "mono".into(),
            Some(2) => "stereo".into(),
            Some(6) => "5.1".into(),
            Some(8) => "7.1".into(),
            Some(n) => format!("{n}ch"),
            None => String::new(),
        }
    }
}

/// Where a newly created child element should go inside a TrackEntry. Right
/// after TrackType keeps the entry close to the order muxers use.
pub fn flag_insert_position(entry: &Element) -> usize {
    let children = entry.children();
    for (i, c) in children.iter().enumerate() {
        if c.id == id::TRACK_TYPE {
            return i + 1;
        }
    }
    for (i, c) in children.iter().enumerate() {
        if c.id != id::CRC32 && c.id != id::VOID {
            return i;
        }
    }
    children.len()
}

/// Names for the ISO 639-2 codes that turn up in real files.
pub fn language_name(code: &str) -> Option<&'static str> {
    let base = code
        .split(['-', '_'])
        .next()
        .unwrap_or(code)
        .to_ascii_lowercase();
    let name = match base.as_str() {
        "eng" | "en" => "English",
        "jpn" | "ja" => "Japanese",
        "spa" | "es" => "Spanish",
        "fra" | "fre" | "fr" => "French",
        "deu" | "ger" | "de" => "German",
        "ita" | "it" => "Italian",
        "por" | "pt" => "Portuguese",
        "rus" | "ru" => "Russian",
        "nld" | "dut" | "nl" => "Dutch",
        "swe" | "sv" => "Swedish",
        "nor" | "no" => "Norwegian",
        "dan" | "da" => "Danish",
        "fin" | "fi" => "Finnish",
        "isl" | "ice" | "is" => "Icelandic",
        "pol" | "pl" => "Polish",
        "ces" | "cze" | "cs" => "Czech",
        "slk" | "slo" | "sk" => "Slovak",
        "hun" | "hu" => "Hungarian",
        "ron" | "rum" | "ro" => "Romanian",
        "ell" | "gre" | "el" => "Greek",
        "tur" | "tr" => "Turkish",
        "ara" | "ar" => "Arabic",
        "heb" | "he" => "Hebrew",
        "hin" | "hi" => "Hindi",
        "ben" | "bn" => "Bengali",
        "tam" | "ta" => "Tamil",
        "tel" | "te" => "Telugu",
        "tha" | "th" => "Thai",
        "vie" | "vi" => "Vietnamese",
        "ind" | "id" => "Indonesian",
        "msa" | "may" | "ms" => "Malay",
        "kor" | "ko" => "Korean",
        "zho" | "chi" | "zh" => "Chinese",
        "ukr" | "uk" => "Ukrainian",
        "bul" | "bg" => "Bulgarian",
        "hrv" | "hr" => "Croatian",
        "srp" | "sr" => "Serbian",
        "slv" | "sl" => "Slovenian",
        "cat" | "ca" => "Catalan",
        "eus" | "baq" | "eu" => "Basque",
        "glg" | "gl" => "Galician",
        "gle" | "ga" => "Irish",
        "cym" | "wel" | "cy" => "Welsh",
        "fas" | "per" | "fa" => "Persian",
        "urd" | "ur" => "Urdu",
        "fil" | "tgl" | "tl" => "Filipino",
        "und" => "Undetermined",
        "mul" => "Multiple",
        "zxx" => "No linguistic content",
        _ => return None,
    };
    Some(name)
}
