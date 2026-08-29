//! Renders the interface into an off screen buffer so the layout can be
//! checked without a terminal.

use std::fs;
use std::path::{Path, PathBuf};

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use mkvtrack::app::App;
use mkvtrack::ebml::{Element, id};
use mkvtrack::ui;

fn fixture(dir: &Path, name: &str) -> PathBuf {
    // A minimal but complete file: header, Segment with Info, Tracks and one
    // cluster. Only the Tracks element matters for rendering.
    let header = Element::master(
        id::EBML_HEAD,
        vec![
            Element::string_elem(0x4282, "matroska"),
            Element::uint_elem(0x4287, 4),
            Element::uint_elem(0x4285, 2),
        ],
    )
    .to_bytes();

    let info = Element::master(
        id::INFO,
        vec![
            Element::uint_elem(id::TIMESTAMP_SCALE, 1_000_000),
            Element::string_elem(id::WRITING_APP, "mkvtrack test"),
            Element::data(id::DURATION, 5_400_000.0f64.to_be_bytes().to_vec()),
            Element::string_elem(id::TITLE, "A Film"),
        ],
    );

    let mut entries = vec![Element::master(
        id::TRACK_ENTRY,
        vec![
            Element::uint_elem(id::TRACK_NUMBER, 1),
            Element::uint_elem(id::TRACK_UID, 1),
            Element::uint_elem(id::TRACK_TYPE, 1),
            Element::string_elem(id::CODEC_ID, "V_MPEGH/ISO/HEVC"),
            Element::string_elem(id::LANGUAGE, "und"),
            Element::master(
                id::VIDEO,
                vec![
                    Element::uint_elem(id::PIXEL_WIDTH, 1920),
                    Element::uint_elem(id::PIXEL_HEIGHT, 1080),
                ],
            ),
        ],
    )];
    let audio = |n: u64, codec: &str, lang: &str, name: &str, ch: u64, default: u64| {
        Element::master(
            id::TRACK_ENTRY,
            vec![
                Element::uint_elem(id::TRACK_NUMBER, n),
                Element::uint_elem(id::TRACK_UID, n),
                Element::uint_elem(id::TRACK_TYPE, 2),
                Element::uint_elem(id::FLAG_DEFAULT, default),
                Element::string_elem(id::CODEC_ID, codec),
                Element::string_elem(id::LANGUAGE, lang),
                Element::string_elem(id::TRACK_NAME, name),
                Element::master(
                    id::AUDIO,
                    vec![
                        Element::uint_elem(id::CHANNELS, ch),
                        Element::data(id::SAMPLING_FREQUENCY, 48000.0f64.to_be_bytes().to_vec()),
                        Element::uint_elem(id::BIT_DEPTH, 24),
                    ],
                ),
            ],
        )
    };
    let subtitle = |n: u64, lang: &str, name: &str, forced: u64| {
        Element::master(
            id::TRACK_ENTRY,
            vec![
                Element::uint_elem(id::TRACK_NUMBER, n),
                Element::uint_elem(id::TRACK_UID, n),
                Element::uint_elem(id::TRACK_TYPE, 0x11),
                Element::uint_elem(id::FLAG_FORCED, forced),
                Element::string_elem(id::CODEC_ID, "S_TEXT/ASS"),
                Element::string_elem(id::LANGUAGE, lang),
                Element::string_elem(id::TRACK_NAME, name),
            ],
        )
    };
    entries.push(audio(2, "A_TRUEHD", "eng", "English Atmos", 8, 1));
    entries.push(audio(3, "A_EAC3", "jpn", "Japanese", 6, 0));
    entries.push(audio(4, "A_AC3", "eng", "Director commentary", 2, 0));
    entries.push(subtitle(5, "eng", "Full", 0));
    entries.push(subtitle(6, "eng", "Signs and songs", 1));

    let mut payload = Vec::new();
    payload.extend(info.to_bytes());
    payload.extend(Element::master(id::TRACKS, entries).to_bytes());
    payload.extend(
        Element::master(
            id::CLUSTER,
            vec![
                Element::uint_elem(id::CLUSTER_TIMESTAMP, 0),
                Element::data(id::SIMPLE_BLOCK, vec![0x81, 0, 0, 0x80, 1, 2, 3]),
            ],
        )
        .to_bytes(),
    );

    let mut out = header;
    mkvtrack::ebml::write_id(&mut out, id::SEGMENT);
    mkvtrack::ebml::write_size(&mut out, payload.len() as u64);
    out.extend(payload);

    let p = dir.join(name);
    fs::write(&p, out).unwrap();
    p
}

fn render(app: &App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| ui::draw(f, app)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..height {
        for x in 0..width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn the_track_list_renders() {
    let dir = std::env::temp_dir().join(format!("mkvtrack-render-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let a = fixture(&dir, "A Film (2019).mkv");
    let b = fixture(&dir, "Another.mkv");

    // No background scan here, so the pane title is settled when it is checked.
    let mut app = App::with_scanner(vec![a, b], false, false);
    app.select_track(1);
    app.make_default();
    let screen = render(&app, 120, 34);
    println!("{screen}");

    for want in [
        "Files (2)",
        "A Film (2019).mkv",
        "TrueHD",
        "English Atmos",
        "eng (English)",
        "jpn",
        "Signs and songs",
        "5.1",
        "7.1",
        "Details",
        "default=",
        "1:30:00",
        "d default",
    ] {
        assert!(
            screen.contains(want),
            "the screen should mention {want:?}:\n{screen}"
        );
    }

    app.show_help = true;
    let help = render(&app, 120, 34);
    println!("{help}");
    assert!(help.contains("Track flags"));

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_narrow_terminal_still_renders() {
    let dir = std::env::temp_dir().join(format!("mkvtrack-render-narrow-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let a = fixture(&dir, "a.mkv");
    let app = App::with_scanner(vec![a], false, false);
    for (w, h) in [(40u16, 10u16), (60, 20), (200, 60)] {
        let screen = render(&app, w, h);
        assert!(!screen.is_empty());
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_directory_is_read_in_the_background() {
    let dir = std::env::temp_dir().join(format!("mkvtrack-scan-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let paths: Vec<PathBuf> = (0..25)
        .map(|i| fixture(&dir, &format!("f{i:02}.mkv")))
        .collect();
    // One file that is not Matroska at all: its error belongs to that row and
    // must not stop the rest of the scan.
    let bad = dir.join("broken.mkv");
    fs::write(&bad, b"not a matroska file at all").unwrap();
    let mut all = paths.clone();
    all.push(bad);

    let mut app = App::new(all.clone(), false);
    // The first file is read up front so the opening frame is not empty.
    assert!(app.current().is_some());

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while app.scan_progress().is_some() {
        app.poll_scan();
        assert!(
            std::time::Instant::now() < deadline,
            "the scan did not finish"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    for (i, entry) in app.files.iter().enumerate() {
        assert!(entry.loaded.is_some(), "file {i} was never read");
    }
    assert!(app.files.last().unwrap().loaded.as_ref().unwrap().is_err());

    // Every file is in memory, so moving through the list touches no disk.
    for i in 0..all.len() {
        app.select_file(1);
        assert_eq!(app.file_sel, (i + 1).min(all.len() - 1));
    }
    app.file_sel = 3;
    app.ensure_loaded();
    assert_eq!(app.tracks().len(), 6);

    fs::remove_dir_all(&dir).unwrap();
}
