//! Writing edited Tracks back to disk.
//!
//! Two strategies:
//!
//! * **In place** - the new Tracks element fits in the space the old one
//!   occupies plus any Void padding that follows it. Nothing else in the file
//!   moves, so a single short write is enough. This is the common case.
//! * **Rewrite** - the Tracks element grew beyond the available space, so the
//!   whole file is copied to a temporary file with everything after Tracks
//!   shifted. Positions recorded in SeekHead, Cues and Cluster/Position are
//!   corrected, and the result is renamed over the original.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::ebml::{self, Element, UNKNOWN, id};
use crate::mkv::{MkvFile, read_at, read_header};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveMode {
    InPlace,
    Rewrite,
}

#[derive(Clone, Debug)]
pub struct SaveReport {
    pub mode: SaveMode,
    pub message: String,
}

/// Payload of the Tracks element, without its own ID and size header.
fn tracks_payload(tracks: &Element) -> Vec<u8> {
    let bytes = tracks.to_bytes();
    let (_, il) = ebml::read_id(&bytes, 0).expect("serialized element");
    let (_, sl) = ebml::read_size(&bytes, il).expect("serialized element");
    bytes[il + sl..].to_vec()
}

/// End of the run of Void elements starting at `pos`.
fn slack_after(f: &mut File, mut pos: u64, limit: u64) -> u64 {
    loop {
        if pos >= limit {
            return pos;
        }
        match read_header(f, pos) {
            Ok((id::VOID, size, hlen)) if size != UNKNOWN && pos + hlen + size <= limit => {
                pos += hlen + size;
            }
            _ => return pos,
        }
    }
}

/// Lays out a Tracks element that occupies exactly `avail` bytes, padding with
/// Void. Returns None when the payload cannot be made to fit.
fn build_region(payload: &[u8], avail: usize) -> Option<Vec<u8>> {
    let idl = ebml::id_len(id::TRACKS);
    let min_sl = ebml::size_len(payload.len() as u64);
    let need = idl + min_sl + payload.len();
    if need > avail {
        return None;
    }
    // A one byte gap cannot hold a Void, so widen the size VINT instead;
    // EBML allows a longer than minimal encoding.
    let size_len = if avail - need == 1 {
        if min_sl >= 8 {
            return None;
        }
        min_sl + 1
    } else {
        min_sl
    };
    let mut out = Vec::with_capacity(avail);
    ebml::write_id(&mut out, id::TRACKS);
    ebml::write_size_with_len(&mut out, payload.len() as u64, size_len);
    out.extend_from_slice(payload);
    let rem = avail - out.len();
    if rem > 0 {
        out.extend_from_slice(&ebml::void_of(rem));
    }
    debug_assert_eq!(out.len(), avail);
    Some(out)
}

fn backup_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".bak");
    PathBuf::from(s)
}

fn make_backup(path: &Path) -> Result<(), String> {
    let bak = backup_path(path);
    if bak.exists() {
        return Ok(());
    }
    std::fs::copy(path, &bak).map_err(|e| format!("backup {}: {e}", bak.display()))?;
    Ok(())
}

/// Refuses to write when the file no longer looks like the one that was read,
/// because every offset we hold would be wrong.
fn check_unchanged(mkv: &MkvFile) -> Result<(), String> {
    let meta = std::fs::metadata(&mkv.path).map_err(|e| format!("{}: {e}", mkv.path.display()))?;
    if meta.len() != mkv.file_len || meta.modified().ok() != mkv.modified {
        return Err("the file changed on disk since it was read; press u to reload it".into());
    }
    Ok(())
}

pub fn save(mkv: &MkvFile, backup: bool) -> Result<SaveReport, String> {
    check_unchanged(mkv)?;
    let payload = tracks_payload(&mkv.tracks);
    let tc = mkv.tracks_child().clone();
    let mut f = File::open(&mkv.path).map_err(|e| format!("{}: {e}", mkv.path.display()))?;
    let slack_end = slack_after(&mut f, tc.end(), mkv.segment_end);
    let avail = (slack_end - tc.start) as usize;

    if let Some(region) = build_region(&payload, avail) {
        if backup {
            make_backup(&mkv.path)?;
        }
        let mut wf = OpenOptions::new()
            .write(true)
            .open(&mkv.path)
            .map_err(|e| format!("{}: {e}", mkv.path.display()))?;
        wf.seek(SeekFrom::Start(tc.start))
            .map_err(|e| e.to_string())?;
        wf.write_all(&region).map_err(|e| e.to_string())?;
        wf.sync_all().map_err(|e| e.to_string())?;
        let used = ebml::id_len(id::TRACKS) + ebml::size_len(payload.len() as u64) + payload.len();
        let pad = avail.saturating_sub(used);
        return Ok(SaveReport {
            mode: SaveMode::InPlace,
            message: format!(
                "saved in place ({} bytes of Tracks, {pad} padding)",
                payload.len()
            ),
        });
    }

    rewrite(mkv, &payload, backup)
}

// ---------------------------------------------------------------------------
// Rewrite path
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Piece {
    /// Copy bytes straight from the source file.
    Copy {
        off: u64,
        len: u64,
    },
    Bytes(Vec<u8>),
}

fn pieces_len(pieces: &[Piece]) -> u64 {
    pieces
        .iter()
        .map(|p| match p {
            Piece::Copy { len, .. } => *len,
            Piece::Bytes(b) => b.len() as u64,
        })
        .sum()
}

/// Maps a position relative to the Segment data start from the old layout to
/// the new one.
fn map_pos(old_rel: &[u64], old_len: &[u64], starts: &[u64], new_len: &[u64], p: u64) -> u64 {
    for i in 0..old_rel.len() {
        if p >= old_rel[i] && p < old_rel[i] + old_len[i] {
            return starts[i] + (p - old_rel[i]);
        }
    }
    // Past the last child, which should not happen in a well formed file:
    // shift by the overall delta and leave anything before the first child
    // where it is.
    let old_end = old_rel.last().copied().unwrap_or(0) + old_len.last().copied().unwrap_or(0);
    let new_end = starts.last().copied().unwrap_or(0) + new_len.last().copied().unwrap_or(0);
    if p >= old_end {
        p + new_end.saturating_sub(old_end)
    } else {
        p
    }
}

/// Rewrites every SeekPosition in a SeekHead.
fn remap_seek_head(el: &Element, f: &mut dyn FnMut(u64) -> u64) -> Element {
    let mut out = el.clone();
    if let Some(children) = out.children_mut() {
        for seek in children.iter_mut() {
            if seek.id != id::SEEK {
                continue;
            }
            if let Some(sc) = seek.children_mut() {
                for e in sc.iter_mut() {
                    if e.id == id::SEEK_POSITION {
                        let old = ebml::read_uint(e.bytes());
                        e.body = ebml::Body::Data(ebml::uint_bytes(f(old)));
                    }
                }
            }
        }
    }
    out
}

/// Rewrites every CueClusterPosition in a Cues element.
fn remap_cues(el: &Element, f: &mut dyn FnMut(u64) -> u64) -> Element {
    let mut out = el.clone();
    if let Some(points) = out.children_mut() {
        for point in points.iter_mut() {
            if point.id != id::CUE_POINT {
                continue;
            }
            let Some(pc) = point.children_mut() else {
                continue;
            };
            for tp in pc.iter_mut() {
                if tp.id != id::CUE_TRACK_POSITIONS {
                    continue;
                }
                let Some(tpc) = tp.children_mut() else {
                    continue;
                };
                for e in tpc.iter_mut() {
                    if e.id == id::CUE_CLUSTER_POSITION {
                        let old = ebml::read_uint(e.bytes());
                        e.body = ebml::Body::Data(ebml::uint_bytes(f(old)));
                    }
                }
            }
        }
    }
    out
}

/// Location of a Cluster's Position element, relative to the cluster payload.
#[derive(Clone, Copy, Debug)]
struct PositionSlot {
    /// Offset of the element ID within the cluster payload.
    off: u64,
    /// Bytes of ID plus size VINT.
    header_len: u64,
    /// Payload length of the Position element.
    size: u64,
}

fn find_position_slot(prefix: &[u8]) -> Option<PositionSlot> {
    let mut pos = 0usize;
    while pos < prefix.len() {
        let (eid, il) = ebml::read_id(prefix, pos)?;
        let (size, sl) = ebml::read_size(prefix, pos + il)?;
        if eid == id::CLUSTER_POSITION {
            return Some(PositionSlot {
                off: pos as u64,
                header_len: (il + sl) as u64,
                size,
            });
        }
        // Blocks start the data; Position always precedes them.
        if eid == id::SIMPLE_BLOCK || eid == id::BLOCK_GROUP || size == UNKNOWN {
            return None;
        }
        pos += il + sl + size as usize;
    }
    None
}

/// Per child state that is expensive to recompute on every iteration.
enum ChildPlan {
    Copy,
    Tracks(Vec<u8>),
    SeekHead(Element),
    Cues(Element),
    Cluster {
        slot: Option<PositionSlot>,
        prefix: Vec<u8>,
    },
}

fn rewrite(mkv: &MkvFile, payload: &[u8], backup: bool) -> Result<SaveReport, String> {
    // Opening a file only walks as far as the clusters, so this is where the
    // rest of the Segment gets located.
    let top = mkv.scan_all().map_err(|why| {
        format!(
            "the Tracks element needs more room, but this file cannot be rewritten safely: {why}"
        )
    })?;
    let tracks_start = mkv.tracks_child().start;
    let tracks_idx = top
        .iter()
        .position(|c| c.start == tracks_start)
        .ok_or("the Tracks element moved while the file was being read")?;

    let mut src = File::open(&mkv.path).map_err(|e| format!("{}: {e}", mkv.path.display()))?;
    let n = top.len();
    let old_rel: Vec<u64> = top
        .iter()
        .map(|c| c.start - mkv.segment_data_start)
        .collect();
    let old_len: Vec<u64> = top.iter().map(|c| c.total_len()).collect();

    let mut new_tracks = Vec::new();
    ebml::write_id(&mut new_tracks, id::TRACKS);
    ebml::write_size(&mut new_tracks, payload.len() as u64);
    new_tracks.extend_from_slice(payload);
    let grew = new_tracks.len() as i64 - old_len[tracks_idx] as i64;

    // Load what we need for each child once.
    let mut plans: Vec<ChildPlan> = Vec::with_capacity(n);
    for (i, c) in top.iter().enumerate() {
        let plan = if i == tracks_idx {
            ChildPlan::Tracks(new_tracks.clone())
        } else {
            match c.id {
                id::SEEK_HEAD | id::CUES => {
                    if c.size > 128 * 1024 * 1024 {
                        return Err(format!("{:#X} element is too large to rewrite", c.id));
                    }
                    let buf = read_at(&mut src, c.data_start(), c.size as usize)
                        .map_err(|e| e.to_string())?;
                    let children = ebml::parse_children(&buf)
                        .map_err(|e| format!("parsing {:#X}: {e}", c.id))?;
                    let el = Element::master(c.id, children);
                    if c.id == id::SEEK_HEAD {
                        ChildPlan::SeekHead(el)
                    } else {
                        ChildPlan::Cues(el)
                    }
                }
                id::CLUSTER => {
                    let want = c.size.min(1024) as usize;
                    let prefix =
                        read_at(&mut src, c.data_start(), want).map_err(|e| e.to_string())?;
                    let slot = find_position_slot(&prefix);
                    ChildPlan::Cluster { slot, prefix }
                }
                _ => ChildPlan::Copy,
            }
        };
        plans.push(plan);
    }

    // Sizes feed back into the positions they encode, so iterate to a fixed
    // point. Two rounds are enough in practice.
    let mut new_len: Vec<u64> = old_len.clone();
    new_len[tracks_idx] = new_tracks.len() as u64;
    let mut pieces: Vec<Vec<Piece>> = Vec::new();

    for round in 0..8 {
        let mut starts = Vec::with_capacity(n);
        let mut acc = 0u64;
        for len in &new_len {
            starts.push(acc);
            acc += len;
        }

        let mut next_pieces: Vec<Vec<Piece>> = Vec::with_capacity(n);
        for i in 0..n {
            let c = &top[i];
            let p = match &plans[i] {
                ChildPlan::Copy => vec![Piece::Copy {
                    off: c.start,
                    len: c.total_len(),
                }],
                ChildPlan::Tracks(b) => vec![Piece::Bytes(b.clone())],
                ChildPlan::SeekHead(el) => {
                    let mut m = |p: u64| map_pos(&old_rel, &old_len, &starts, &new_len, p);
                    vec![Piece::Bytes(remap_seek_head(el, &mut m).to_bytes())]
                }
                ChildPlan::Cues(el) => {
                    let mut m = |p: u64| map_pos(&old_rel, &old_len, &starts, &new_len, p);
                    vec![Piece::Bytes(remap_cues(el, &mut m).to_bytes())]
                }
                ChildPlan::Cluster { slot, prefix } => match slot {
                    None => vec![Piece::Copy {
                        off: c.start,
                        len: c.total_len(),
                    }],
                    Some(slot) => cluster_pieces(c, slot, prefix, starts[i]),
                },
            };
            next_pieces.push(p);
        }

        let sizes: Vec<u64> = next_pieces.iter().map(|p| pieces_len(p)).collect();
        let stable = sizes == new_len;
        new_len = sizes;
        pieces = next_pieces;
        if stable {
            break;
        }
        if round == 7 {
            return Err("could not settle the new file layout".into());
        }
    }

    let segment_payload: u64 = new_len.iter().sum();

    // Write to a sibling temporary file, then rename over the original.
    let dir = mkv.path.parent().unwrap_or_else(|| Path::new("."));
    let stem = mkv
        .path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{stem}.mkvtrack-tmp"));

    let result = (|| -> Result<(), String> {
        let out_file = File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        let mut out = BufWriter::with_capacity(1 << 20, out_file);

        copy_range(&mut src, &mut out, 0, mkv.segment_start)?;
        let mut hdr = Vec::new();
        ebml::write_id(&mut hdr, id::SEGMENT);
        ebml::write_size_with_len(&mut hdr, segment_payload, 8);
        out.write_all(&hdr).map_err(|e| e.to_string())?;

        for child in &pieces {
            for piece in child {
                match piece {
                    Piece::Copy { off, len } => copy_range(&mut src, &mut out, *off, *len)?,
                    Piece::Bytes(b) => out.write_all(b).map_err(|e| e.to_string())?,
                }
            }
        }
        let f = out.into_inner().map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        Ok(())
    })();

    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    if let Ok(meta) = std::fs::metadata(&mkv.path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    if backup {
        make_backup(&mkv.path)?;
    }
    std::fs::rename(&tmp, &mkv.path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("replacing {}: {e}", mkv.path.display())
    })?;

    Ok(SaveReport {
        mode: SaveMode::Rewrite,
        message: format!(
            "rewrote {} (Tracks grew by {grew} bytes, {} bytes copied)",
            mkv.path.file_name().unwrap_or_default().to_string_lossy(),
            mkv.file_len
        ),
    })
}

/// Pieces for a Cluster whose Position element must be corrected.
fn cluster_pieces(
    c: &crate::mkv::TopChild,
    slot: &PositionSlot,
    prefix: &[u8],
    new_rel: u64,
) -> Vec<Piece> {
    let value_off = c.data_start() + slot.off + slot.header_len;
    if let Some(bytes) = ebml::uint_bytes_fixed(new_rel, slot.size as usize) {
        // Same width: patch the value and leave every length alone.
        return vec![
            Piece::Copy {
                off: c.start,
                len: value_off - c.start,
            },
            Piece::Bytes(bytes),
            Piece::Copy {
                off: value_off + slot.size,
                len: c.end() - (value_off + slot.size),
            },
        ];
    }
    // The value no longer fits, so the cluster header is rebuilt one size up.
    let value = ebml::uint_bytes(new_rel);
    let delta = value.len() as u64 - slot.size;
    let new_payload = c.size + delta;
    let mut head = Vec::new();
    ebml::write_id(&mut head, id::CLUSTER);
    ebml::write_size(&mut head, new_payload);
    head.extend_from_slice(&prefix[..slot.off as usize]);
    ebml::write_id(&mut head, id::CLUSTER_POSITION);
    ebml::write_size(&mut head, value.len() as u64);
    head.extend_from_slice(&value);
    let tail_off = c.data_start() + slot.off + slot.header_len + slot.size;
    vec![
        Piece::Bytes(head),
        Piece::Copy {
            off: tail_off,
            len: c.end() - tail_off,
        },
    ]
}

fn copy_range<W: Write>(src: &mut File, out: &mut W, off: u64, len: u64) -> Result<(), String> {
    src.seek(SeekFrom::Start(off)).map_err(|e| e.to_string())?;
    let mut left = len;
    let mut buf = vec![0u8; 1 << 20];
    while left > 0 {
        let want = left.min(buf.len() as u64) as usize;
        let got = src.read(&mut buf[..want]).map_err(|e| e.to_string())?;
        if got == 0 {
            return Err("unexpected end of file while copying".into());
        }
        out.write_all(&buf[..got]).map_err(|e| e.to_string())?;
        left -= got as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_fills_exactly() {
        let payload = vec![0u8; 100];
        for avail in [108usize, 109, 110, 111, 200] {
            let r = build_region(&payload, avail).expect("fits");
            assert_eq!(r.len(), avail, "avail {avail}");
            let (eid, il) = ebml::read_id(&r, 0).unwrap();
            let (size, sl) = ebml::read_size(&r, il).unwrap();
            assert_eq!(eid, id::TRACKS);
            assert_eq!(size, 100);
            // Whatever is left over must be a single valid Void.
            let used = il + sl + 100;
            if used < avail {
                let (vid, vil) = ebml::read_id(&r, used).unwrap();
                let (vsize, vsl) = ebml::read_size(&r, used + vil).unwrap();
                assert_eq!(vid, id::VOID);
                assert_eq!(used + vil + vsl + vsize as usize, avail);
            }
        }
    }

    #[test]
    fn region_rejects_a_payload_that_is_too_big() {
        assert!(build_region(&[0u8; 100], 104).is_none());
    }
}
