//! End to end checks: build Matroska files, edit their track flags through
//! the same code path the interface uses, and verify that every position
//! recorded in the file still points where it should.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use mkvtrack::app::App;
use mkvtrack::ebml::{self, Element, id};
use mkvtrack::edit::{self, SaveMode};
use mkvtrack::mkv::MkvFile;

// ---------------------------------------------------------------------------
// Building test files
// ---------------------------------------------------------------------------

fn ebml_header() -> Vec<u8> {
    Element::master(
        id::EBML_HEAD,
        vec![
            Element::uint_elem(0x4286, 1),
            Element::uint_elem(0x42F7, 1),
            Element::uint_elem(0x42F2, 4),
            Element::uint_elem(0x42F3, 8),
            Element::string_elem(0x4282, "matroska"),
            Element::uint_elem(0x4287, 4),
            Element::uint_elem(0x4285, 2),
        ],
    )
    .to_bytes()
}

fn info() -> Element {
    Element::master(
        id::INFO,
        vec![
            Element::uint_elem(id::TIMESTAMP_SCALE, 1_000_000),
            Element::string_elem(id::MUXING_APP, "mkvtrack test"),
            Element::string_elem(id::WRITING_APP, "mkvtrack test"),
            Element::data(id::DURATION, 5000.0f64.to_be_bytes().to_vec()),
        ],
    )
}

/// A track entry. `flags` are appended verbatim so a test can leave them out.
fn track(
    number: u64,
    ttype: u64,
    codec: &str,
    lang: &str,
    name: &str,
    flags: Vec<Element>,
) -> Element {
    let mut children = vec![
        Element::uint_elem(id::TRACK_NUMBER, number),
        Element::uint_elem(id::TRACK_UID, 0xF000 + number),
        Element::uint_elem(id::TRACK_TYPE, ttype),
    ];
    children.extend(flags);
    children.push(Element::string_elem(id::CODEC_ID, codec));
    children.push(Element::string_elem(id::LANGUAGE, lang));
    if !name.is_empty() {
        children.push(Element::string_elem(id::TRACK_NAME, name));
    }
    if ttype == 2 {
        children.push(Element::master(
            id::AUDIO,
            vec![
                Element::uint_elem(id::CHANNELS, 6),
                Element::data(id::SAMPLING_FREQUENCY, 48000.0f64.to_be_bytes().to_vec()),
            ],
        ));
    }
    Element::master(id::TRACK_ENTRY, children)
}

/// Tracks with explicit FlagDefault on every entry: edits fit in place.
fn tracks_explicit() -> Element {
    Element::master(
        id::TRACKS,
        vec![
            track(
                1,
                1,
                "V_MPEG4/ISO/AVC",
                "und",
                "",
                vec![Element::uint_elem(id::FLAG_DEFAULT, 1)],
            ),
            track(
                2,
                2,
                "A_EAC3",
                "eng",
                "English",
                vec![Element::uint_elem(id::FLAG_DEFAULT, 1)],
            ),
            track(
                3,
                2,
                "A_AAC",
                "jpn",
                "Japanese",
                vec![Element::uint_elem(id::FLAG_DEFAULT, 0)],
            ),
            track(
                4,
                0x11,
                "S_TEXT/UTF8",
                "eng",
                "Full",
                vec![Element::uint_elem(id::FLAG_DEFAULT, 1)],
            ),
            track(
                5,
                0x11,
                "S_TEXT/UTF8",
                "eng",
                "Signs",
                vec![Element::uint_elem(id::FLAG_DEFAULT, 0)],
            ),
        ],
    )
}

/// Tracks with no flag elements at all: any edit has to add bytes.
fn tracks_implicit() -> Element {
    Element::master(
        id::TRACKS,
        vec![
            track(1, 1, "V_MPEG4/ISO/AVC", "und", "", vec![]),
            track(2, 2, "A_EAC3", "eng", "English", vec![]),
            track(3, 2, "A_AAC", "jpn", "Japanese", vec![]),
            track(4, 0x11, "S_TEXT/UTF8", "eng", "Full", vec![]),
            track(5, 0x11, "S_TEXT/UTF8", "eng", "Signs", vec![]),
        ],
    )
}

fn cluster(timestamp: u64, position: u64, payload_size: usize, pos_width: usize) -> Element {
    let mut block = vec![0x81, 0x00, 0x00, 0x80];
    block.extend(std::iter::repeat_n(0xAB, payload_size));
    Element::master(
        id::CLUSTER,
        vec![
            Element::uint_elem(id::CLUSTER_TIMESTAMP, timestamp),
            Element::data(
                id::CLUSTER_POSITION,
                ebml::uint_bytes_fixed(position, pos_width)
                    .expect("cluster position fits the chosen width"),
            ),
            Element::data(id::SIMPLE_BLOCK, block),
        ],
    )
}

fn cue(time: u64, track_number: u64, cluster_position: u64) -> Element {
    Element::master(
        id::CUE_POINT,
        vec![
            Element::uint_elem(0xB3, time),
            Element::master(
                id::CUE_TRACK_POSITIONS,
                vec![
                    Element::uint_elem(0xF7, track_number),
                    Element::data(
                        id::CUE_CLUSTER_POSITION,
                        ebml::uint_bytes_fixed(cluster_position, 8).unwrap(),
                    ),
                ],
            ),
        ],
    )
}

fn seek_entry(seek_id: u64, position: u64) -> Element {
    let mut id_bytes = Vec::new();
    ebml::write_id(&mut id_bytes, seek_id);
    Element::master(
        id::SEEK,
        vec![
            Element::data(id::SEEK_ID, id_bytes),
            Element::data(
                id::SEEK_POSITION,
                ebml::uint_bytes_fixed(position, 8).unwrap(),
            ),
        ],
    )
}

/// Assembles a complete file. Seek and cue positions are stored eight bytes
/// wide so the layout does not change between the two passes; the cluster
/// Position width is chosen by the caller, which lets a test force the
/// "value no longer fits" branch of the rewriter.
fn build(tracks: Element, void_after_tracks: usize, pos_width: usize, block: usize) -> Vec<u8> {
    let mut positions = (0u64, 0u64, 0u64, 0u64, 0u64); // info, tracks, c1, c2, cues

    for _pass in 0..2 {
        let seek_head = Element::master(
            id::SEEK_HEAD,
            vec![
                seek_entry(id::INFO, positions.0),
                seek_entry(id::TRACKS, positions.1),
                seek_entry(id::CUES, positions.4),
            ],
        );
        let mut acc = 0u64;
        let mut next = |len: usize, slot: &mut u64| {
            *slot = acc;
            acc += len as u64;
        };
        let mut sh_pos = 0;
        next(seek_head.to_bytes().len(), &mut sh_pos);
        let mut info_pos = 0;
        next(info().to_bytes().len(), &mut info_pos);
        let mut tracks_pos = 0;
        next(tracks.to_bytes().len() + void_after_tracks, &mut tracks_pos);
        let mut c1_pos = 0;
        next(
            cluster(0, positions.2, block, pos_width).to_bytes().len(),
            &mut c1_pos,
        );
        let mut c2_pos = 0;
        next(
            cluster(1000, positions.3, block, pos_width)
                .to_bytes()
                .len(),
            &mut c2_pos,
        );
        let mut cues_pos = 0;
        next(0, &mut cues_pos);
        positions = (info_pos, tracks_pos, c1_pos, c2_pos, cues_pos);
    }

    let seek_head = Element::master(
        id::SEEK_HEAD,
        vec![
            seek_entry(id::INFO, positions.0),
            seek_entry(id::TRACKS, positions.1),
            seek_entry(id::CUES, positions.4),
        ],
    );
    let cues = Element::master(
        id::CUES,
        vec![cue(0, 1, positions.2), cue(1000, 1, positions.3)],
    );

    let mut payload = Vec::new();
    payload.extend(seek_head.to_bytes());
    payload.extend(info().to_bytes());
    payload.extend(tracks.to_bytes());
    if void_after_tracks > 0 {
        payload.extend(ebml::void_of(void_after_tracks));
    }
    payload.extend(cluster(0, positions.2, block, pos_width).to_bytes());
    payload.extend(cluster(1000, positions.3, block, pos_width).to_bytes());
    payload.extend(cues.to_bytes());

    let mut out = ebml_header();
    ebml::write_id(&mut out, id::SEGMENT);
    ebml::write_size(&mut out, payload.len() as u64);
    out.extend(payload);
    out
}

fn build_file(tracks: Element, void_after_tracks: usize) -> Vec<u8> {
    build(tracks, void_after_tracks, 8, 400)
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

fn read_bytes(path: &Path) -> Vec<u8> {
    let mut buf = Vec::new();
    fs::File::open(path).unwrap().read_to_end(&mut buf).unwrap();
    buf
}

/// Checks every cross reference in the file: the Segment covers the file, the
/// children tile it exactly, SeekHead entries land on the element they name,
/// cues land on clusters, and each cluster's Position matches where it is.
fn verify(path: &Path) {
    let buf = read_bytes(path);
    let mkv = MkvFile::open(path).expect("reopen");
    // Opening stops at the clusters, so ask for the whole list here.
    let top = mkv
        .scan_all()
        .unwrap_or_else(|e| panic!("the scan of {} stopped early: {e}", path.display()));
    assert_eq!(
        mkv.segment_end,
        buf.len() as u64,
        "the Segment does not reach the end of {}",
        path.display()
    );

    let base = mkv.segment_data_start;
    let mut expect = base;
    for c in &top {
        assert_eq!(c.start, expect, "gap before element {:#X}", c.id);
        expect = c.end();
    }
    assert_eq!(expect, mkv.segment_end, "children do not fill the Segment");

    let id_at = |abs: u64| -> u64 { ebml::read_id(&buf, abs as usize).expect("element ID").0 };

    for c in &top {
        let payload = &buf[c.data_start() as usize..c.end() as usize];
        match c.id {
            id::SEEK_HEAD => {
                let el = Element::master(c.id, ebml::parse_children(payload).unwrap());
                for seek in el.children().iter().filter(|s| s.id == id::SEEK) {
                    let want = ebml::read_id(seek.find(id::SEEK_ID).unwrap().bytes(), 0)
                        .unwrap()
                        .0;
                    let pos = ebml::read_uint(seek.find(id::SEEK_POSITION).unwrap().bytes());
                    assert_eq!(
                        id_at(base + pos),
                        want,
                        "SeekHead entry for {want:#X} points at the wrong element"
                    );
                }
            }
            id::CUES => {
                let el = Element::master(c.id, ebml::parse_children(payload).unwrap());
                for point in el.children() {
                    for tp in point.children() {
                        if tp.id != id::CUE_TRACK_POSITIONS {
                            continue;
                        }
                        let pos =
                            ebml::read_uint(tp.find(id::CUE_CLUSTER_POSITION).unwrap().bytes());
                        assert_eq!(
                            id_at(base + pos),
                            id::CLUSTER,
                            "cue points at {pos}, which is not a Cluster"
                        );
                    }
                }
            }
            id::CLUSTER => {
                let el = Element::master(c.id, ebml::parse_children(payload).unwrap());
                if let Some(p) = el.find(id::CLUSTER_POSITION) {
                    assert_eq!(
                        ebml::read_uint(p.bytes()),
                        c.start - base,
                        "cluster Position does not match where the cluster is"
                    );
                }
            }
            _ => {}
        }
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mkvtrack-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, bytes).unwrap();
    p
}

fn flags(path: &Path) -> Vec<(u64, bool, bool)> {
    MkvFile::open(path)
        .unwrap()
        .tracks_view()
        .iter()
        .map(|t| (t.number, t.default.value, t.forced.value))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn the_generated_fixture_is_consistent() {
    let dir = temp_dir("fixture");
    let p = write_file(&dir, "a.mkv", &build_file(tracks_explicit(), 64));
    verify(&p);
    let mkv = MkvFile::open(&p).unwrap();
    assert_eq!(mkv.tracks_view().len(), 5);
    assert_eq!(mkv.info.writing_app.as_deref(), Some("mkvtrack test"));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn changing_an_existing_flag_writes_in_place() {
    let dir = temp_dir("inplace");
    let bytes = build_file(tracks_explicit(), 0);
    let p = write_file(&dir, "a.mkv", &bytes);
    let before = fs::metadata(&p).unwrap().len();

    let mut mkv = MkvFile::open(&p).unwrap();
    // Make track 3 the default audio track.
    let entries = mkv.tracks.children_mut().unwrap();
    for e in entries.iter_mut() {
        if e.get_uint(id::TRACK_TYPE) == Some(2) {
            let want = e.get_uint(id::TRACK_NUMBER) == Some(3);
            e.set_uint(id::FLAG_DEFAULT, want as u64, 3);
        }
    }
    let report = edit::save(&mkv, false).unwrap();
    assert_eq!(report.mode, SaveMode::InPlace, "{}", report.message);
    assert_eq!(
        fs::metadata(&p).unwrap().len(),
        before,
        "the file changed size"
    );

    verify(&p);
    let got = flags(&p);
    assert_eq!(got[1], (2, false, false));
    assert_eq!(got[2], (3, true, false));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_growing_tracks_element_uses_the_void_that_follows_it() {
    let dir = temp_dir("void");
    let p = write_file(&dir, "a.mkv", &build_file(tracks_implicit(), 64));
    let before = fs::metadata(&p).unwrap().len();

    let mut app = App::new(vec![p.clone()], false);
    app.select_track(4); // the "Signs" subtitle track
    app.toggle_flag(id::FLAG_FORCED);
    app.make_default();
    app.save_current();
    assert!(!app.status_error, "{}", app.status);
    assert!(app.status.contains("in place"), "{}", app.status);
    assert_eq!(
        fs::metadata(&p).unwrap().len(),
        before,
        "the file changed size"
    );

    verify(&p);
    let got = flags(&p);
    assert_eq!(
        got[3],
        (4, false, false),
        "the other subtitle track lost its default"
    );
    assert_eq!(got[4], (5, true, true));
    // Audio tracks are untouched: both are still implicitly default.
    assert!(got[1].1);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_growing_tracks_element_without_room_rewrites_the_file() {
    let dir = temp_dir("rewrite");
    let p = write_file(&dir, "a.mkv", &build_file(tracks_implicit(), 0));
    let before = fs::metadata(&p).unwrap().len();

    let mut mkv = MkvFile::open(&p).unwrap();
    let entries = mkv.tracks.children_mut().unwrap();
    for e in entries.iter_mut() {
        if e.get_uint(id::TRACK_TYPE) == Some(2) {
            let want = e.get_uint(id::TRACK_NUMBER) == Some(3);
            e.set_uint(id::FLAG_DEFAULT, want as u64, 3);
        }
        if e.get_uint(id::TRACK_NUMBER) == Some(5) {
            e.set_uint(id::FLAG_FORCED, 1, 3);
            // Enough extra bytes that the file has to grow even though the
            // rewriter also compacts the padded seek and cue positions.
            e.set_string(id::TRACK_NAME, &"Signs and songs ".repeat(20), 3);
        }
    }
    let report = edit::save(&mkv, true).unwrap();
    assert_eq!(report.mode, SaveMode::Rewrite, "{}", report.message);

    let after = fs::metadata(&p).unwrap().len();
    assert!(
        after > before,
        "the file should have grown: {before} -> {after}"
    );
    verify(&p);

    let got = flags(&p);
    assert_eq!(got[1], (2, false, false));
    assert_eq!(got[2], (3, true, false));
    assert_eq!(got[4], (5, true, true));

    // The backup is the original file, untouched.
    let bak = dir.join("a.mkv.bak");
    assert_eq!(read_bytes(&bak), build_file(tracks_implicit(), 0));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn editing_names_and_languages_survives_a_round_trip() {
    let dir = temp_dir("props");
    let p = write_file(&dir, "a.mkv", &build_file(tracks_explicit(), 200));

    let mut app = App::new(vec![p.clone()], false);
    app.select_track(4);
    app.start_input(mkvtrack::app::InputTarget::Name);
    app.input.as_mut().unwrap().value = "Signs and songs".into();
    app.commit_input();
    app.start_input(mkvtrack::app::InputTarget::Language);
    app.input.as_mut().unwrap().value = "jpn".into();
    app.commit_input();
    app.save_current();
    assert!(!app.status_error, "{}", app.status);

    verify(&p);
    let mkv = MkvFile::open(&p).unwrap();
    let t = &mkv.tracks_view()[4];
    assert_eq!(t.name.as_deref(), Some("Signs and songs"));
    assert_eq!(t.language, "jpn");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn every_other_track_property_is_preserved() {
    let dir = temp_dir("preserve");
    let p = write_file(&dir, "a.mkv", &build_file(tracks_explicit(), 64));
    let describe = |path: &Path| -> Vec<String> {
        MkvFile::open(path)
            .unwrap()
            .tracks_view()
            .iter()
            .map(|t| {
                format!(
                    "{} {} {} {} {:?} {:?} {} {} {} {:?}",
                    t.number,
                    t.uid,
                    t.ttype,
                    t.codec_id,
                    t.name,
                    t.language,
                    t.default.value,
                    t.forced.value,
                    t.enabled.value,
                    t.audio.as_ref().map(|a| (a.channels, a.sampling_frequency)),
                )
            })
            .collect()
    };
    let before = describe(&p);

    let mut app = App::new(vec![p.clone()], false);
    app.select_track(1);
    app.toggle_flag(id::FLAG_COMMENTARY);
    app.toggle_flag(id::FLAG_COMMENTARY); // back to where it started
    app.save_current();
    assert!(!app.status_error, "{}", app.status);

    assert_eq!(before, describe(&p));
    // The flag was written out explicitly, but its value is the default.
    let t = &MkvFile::open(&p).unwrap().tracks_view()[1];
    assert!(!t.commentary.value);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_cluster_position_that_outgrows_its_field_is_widened() {
    let dir = temp_dir("widen");
    // Small file with one byte cluster positions: after the edit the first
    // cluster sits past 255, so the Position element has to get wider.
    let tracks = Element::master(
        id::TRACKS,
        vec![
            track(1, 1, "V_VP9", "und", "", vec![]),
            track(2, 2, "A_OPUS", "eng", "En", vec![]),
        ],
    );
    let bytes = build(tracks, 0, 1, 20);
    let p = write_file(&dir, "a.mkv", &bytes);

    let first_cluster = MkvFile::open(&p)
        .unwrap()
        .top
        .iter()
        .find(|c| c.id == id::CLUSTER)
        .map(|c| c.start - MkvFile::open(&p).unwrap().segment_data_start)
        .unwrap();
    assert!(
        first_cluster < 256,
        "the fixture should start with a one byte position"
    );

    let mut mkv = MkvFile::open(&p).unwrap();
    let entries = mkv.tracks.children_mut().unwrap();
    entries[1].set_string(id::TRACK_NAME, &"x".repeat(200), 3);
    let report = edit::save(&mkv, false).unwrap();
    assert_eq!(report.mode, SaveMode::Rewrite, "{}", report.message);

    verify(&p);
    let mkv = MkvFile::open(&p).unwrap();
    let cluster_rel = mkv
        .top
        .iter()
        .find(|c| c.id == id::CLUSTER)
        .map(|c| c.start - mkv.segment_data_start)
        .unwrap();
    assert!(
        cluster_rel > 255,
        "the cluster should have moved past a one byte position"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_crc32_inside_tracks_is_recomputed() {
    let dir = temp_dir("crc");
    // Start with a deliberately wrong checksum: saving must correct it.
    let mut children = vec![Element::data(id::CRC32, vec![0, 0, 0, 0])];
    children.extend(tracks_explicit().children().to_vec());
    let p = write_file(
        &dir,
        "a.mkv",
        &build_file(Element::master(id::TRACKS, children), 64),
    );

    let mut app = App::new(vec![p.clone()], false);
    app.select_track(2);
    app.make_default();
    app.save_current();
    assert!(!app.status_error, "{}", app.status);
    verify(&p);

    let buf = read_bytes(&p);
    let mkv = MkvFile::open(&p).unwrap();
    let c = mkv.tracks_child();
    let payload = &buf[c.data_start() as usize..c.end() as usize];
    // The CRC-32 element comes first and covers everything after it.
    assert_eq!(payload[0], 0xBF);
    assert_eq!(payload[1], 0x84);
    let stored = u32::from_le_bytes([payload[2], payload[3], payload[4], payload[5]]);
    assert_eq!(
        stored,
        ebml::crc32(&payload[6..]),
        "the checksum was not recomputed"
    );
    assert_ne!(stored, 0);
    eprintln!("crc fixture: {}", p.display());
    // Left in place so mkvinfo can check it independently.
}

/// Runs against a real file when one is named in MKVTRACK_TEST_MKV.
#[test]
fn a_real_file_can_be_edited() {
    let Ok(src) = std::env::var("MKVTRACK_TEST_MKV") else {
        eprintln!("skipped: set MKVTRACK_TEST_MKV to a Matroska file to run this");
        return;
    };
    let dir = temp_dir("real");
    let p = dir.join("sample.mkv");
    fs::copy(&src, &p).unwrap();
    verify(&p);

    let mut app = App::new(vec![p.clone()], false);
    let audio: Vec<usize> = app
        .tracks()
        .iter()
        .enumerate()
        .filter(|(_, t)| t.ttype == 2)
        .map(|(i, _)| i)
        .collect();
    assert!(
        audio.len() >= 2,
        "the test file needs at least two audio tracks"
    );

    app.select_track(audio[1] as i32);
    app.make_default();
    let subs: Vec<usize> = app
        .tracks()
        .iter()
        .enumerate()
        .filter(|(_, t)| t.ttype == 0x11)
        .map(|(i, _)| i)
        .collect();
    if let Some(last) = subs.last() {
        app.track_sel = *last;
        app.toggle_flag(id::FLAG_FORCED);
        app.make_default();
    }
    app.save_current();
    assert!(!app.status_error, "{}", app.status);
    verify(&p);

    let tracks = MkvFile::open(&p).unwrap().tracks_view();
    assert!(tracks[audio[1]].default.value);
    assert!(!tracks[audio[0]].default.value);
    if let Some(last) = subs.last() {
        assert!(tracks[*last].forced.value);
        assert!(tracks[*last].default.value);
    }
    // Second phase: grow the Tracks element well past any padding the muxer
    // left, which forces the whole file to be rewritten.
    let mut mkv = MkvFile::open(&p).unwrap();
    let entries = mkv.tracks.children_mut().unwrap();
    let last = entries.len() - 1;
    entries[last].set_string(id::TRACK_NAME, &"long name ".repeat(2000), 3);
    let report = edit::save(&mkv, false).unwrap();
    assert_eq!(report.mode, SaveMode::Rewrite, "{}", report.message);
    verify(&p);
    let tracks = MkvFile::open(&p).unwrap().tracks_view();
    assert_eq!(
        tracks[tracks.len() - 1].name.as_deref(),
        Some(&*"long name ".repeat(2000))
    );

    eprintln!("edited {} -> {}", src, p.display());
    // Left in place on purpose so the file can be inspected by other tools.
}
