//! mkvtrack - inspect and edit Matroska track flags from the terminal.
//!
//! Everything is done with this program's own EBML reader and writer; no
//! external tools are involved.

use mkvtrack::{app, ebml, mkv, ui};

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use app::{App, Focus, InputTarget};
use ebml::id;
use mkv::MkvFile;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "\
mkvtrack - inspect and edit Matroska audio and subtitle tracks

USAGE:
    mkvtrack [OPTIONS] [PATH]...

PATH may be a Matroska file or a directory of them. With no PATH the
current directory is used.

OPTIONS:
    -r, --recursive   descend into subdirectories
    -b, --backup      copy each file to <name>.bak before the first write
    -l, --list        print the tracks and exit, without the interface
    -h, --help        show this text
    -V, --version     show the version

KEYS:
    up/down, j/k  move          Tab   switch pane        [ ]  previous/next file
    d  make default             D     clear default
    f  forced                   e     enabled
    h  hearing impaired         v     visual impaired
    t  text descriptions        o     original           c    commentary
    n  edit name                l     edit language
    s  save                     S     save all           u    discard changes
    ?  help                     q     quit
";

struct Options {
    paths: Vec<PathBuf>,
    recursive: bool,
    backup: bool,
    list: bool,
}

fn parse_args() -> Result<Options, String> {
    let mut opts = Options {
        paths: Vec::new(),
        recursive: false,
        backup: false,
        list: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("mkvtrack {VERSION}");
                std::process::exit(0);
            }
            "-r" | "--recursive" => opts.recursive = true,
            "-b" | "--backup" => opts.backup = true,
            "-l" | "--list" => opts.list = true,
            "--" => opts.paths.extend(args.by_ref().map(PathBuf::from)),
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unknown option {other}"));
            }
            other => opts.paths.push(PathBuf::from(other)),
        }
    }
    if opts.paths.is_empty() {
        opts.paths.push(PathBuf::from("."));
    }
    Ok(opts)
}

fn is_matroska(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) => {
            let e = e.to_ascii_lowercase();
            e == "mkv" || e == "mka" || e == "mks" || e == "mk3d" || e == "webm"
        }
        None => false,
    }
}

fn collect(paths: &[PathBuf], recursive: bool) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for p in paths {
        let meta = std::fs::metadata(p).map_err(|e| format!("{}: {e}", p.display()))?;
        if meta.is_dir() {
            walk(p, recursive, &mut out)?;
        } else {
            out.push(p.clone());
        }
    }
    out.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    out.dedup();
    Ok(out)
}

fn walk(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let ty = entry.file_type().map_err(|e| e.to_string())?;
        if ty.is_dir() {
            if recursive {
                walk(&path, recursive, out)?;
            }
        } else if is_matroska(&path) {
            out.push(path);
        }
    }
    Ok(())
}

fn main() {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("mkvtrack: {e}");
            std::process::exit(2);
        }
    };
    let files = match collect(&opts.paths, opts.recursive) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("mkvtrack: {e}");
            std::process::exit(1);
        }
    };
    if files.is_empty() {
        eprintln!("mkvtrack: no Matroska files found");
        std::process::exit(1);
    }

    if opts.list {
        list(&files);
        return;
    }

    if let Err(e) = run(files, opts.backup) {
        eprintln!("mkvtrack: {e}");
        std::process::exit(1);
    }
}

fn list(files: &[PathBuf]) {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for path in files {
        match MkvFile::open(path) {
            Ok(m) => {
                let _ = writeln!(out, "{}", path.display());
                let mut bits: Vec<String> = Vec::new();
                if let Some(t) = &m.info.title {
                    bits.push(format!("title {t}"));
                }
                if let Some(d) = m.info.duration_secs {
                    bits.push(format!("{:.0}s", d));
                }
                if let Some(w) = &m.info.writing_app {
                    bits.push(w.clone());
                }
                if !bits.is_empty() {
                    let _ = writeln!(out, "  ({})", bits.join(", "));
                }
                for t in m.tracks_view() {
                    let _ = writeln!(out, "{}", ui::plain_line(&t));
                }
            }
            Err(e) => {
                let _ = writeln!(out, "{}: {e}", path.display());
            }
        }
        let _ = writeln!(out);
    }
}

fn run(files: Vec<PathBuf>, backup: bool) -> Result<(), String> {
    let mut app = App::new(files, backup);
    app.info("ready");

    enable_raw_mode().map_err(|e| e.to_string())?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|e| e.to_string())?;

    // Leave the terminal usable if something goes wrong.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        hook(info);
    }));

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|e| e.to_string())?;
    let result = event_loop(&mut terminal, &mut app);

    disable_raw_mode().map_err(|e| e.to_string())?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).map_err(|e| e.to_string())?;
    terminal.show_cursor().map_err(|e| e.to_string())?;
    result
}

fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<(), String> {
    loop {
        terminal
            .draw(|f| ui::draw(f, app))
            .map_err(|e| e.to_string())?;
        // Wake up more often while the directory is still being read, so the
        // file list fills in smoothly.
        let wait = if app.scan_progress().is_some() {
            Duration::from_millis(60)
        } else {
            Duration::from_millis(250)
        };
        if !event::poll(wait).map_err(|e| e.to_string())? {
            app.poll_scan();
            continue;
        }
        app.poll_scan();
        match event::read().map_err(|e| e.to_string())? {
            Event::Key(key) if key.kind == KeyEventKind::Press => handle_key(app, key),
            _ => {}
        }
        if app.should_quit {
            return Ok(());
        }
    }
}

fn handle_key(app: &mut App, key: KeyEvent) {
    // Text entry takes every key except Enter and Esc.
    if app.input.is_some() {
        match key.code {
            KeyCode::Enter => app.commit_input(),
            KeyCode::Esc => {
                app.input = None;
                app.info("cancelled");
            }
            KeyCode::Backspace => {
                if let Some(i) = app.input.as_mut() {
                    i.value.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(i) = app.input.as_mut() {
                    i.value.push(c);
                }
            }
            _ => {}
        }
        return;
    }

    if app.confirm_quit {
        match key.code {
            KeyCode::Char('s') | KeyCode::Char('S') => {
                app.save_all();
                if app.dirty_count() == 0 {
                    app.should_quit = true;
                } else {
                    app.confirm_quit = false;
                }
            }
            KeyCode::Char('q') | KeyCode::Char('y') => app.should_quit = true,
            KeyCode::Esc | KeyCode::Char('n') => app.confirm_quit = false,
            _ => {}
        }
        return;
    }

    if app.show_help {
        app.show_help = false;
        return;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            if app.dirty_count() > 0 {
                app.confirm_quit = true;
            } else {
                app.should_quit = true;
            }
        }
        KeyCode::Char('?') | KeyCode::F(1) => app.show_help = true,
        KeyCode::Tab | KeyCode::BackTab => {
            app.focus = match app.focus {
                Focus::Files => Focus::Tracks,
                Focus::Tracks => Focus::Files,
            };
        }
        KeyCode::Down | KeyCode::Char('j') => match app.focus {
            Focus::Files => app.select_file(1),
            Focus::Tracks => app.select_track(1),
        },
        KeyCode::Up | KeyCode::Char('k') => match app.focus {
            Focus::Files => app.select_file(-1),
            Focus::Tracks => app.select_track(-1),
        },
        KeyCode::Home => match app.focus {
            Focus::Files => app.select_file(i32::MIN / 2),
            Focus::Tracks => app.select_track(i32::MIN / 2),
        },
        KeyCode::End => match app.focus {
            Focus::Files => app.select_file(i32::MAX / 2),
            Focus::Tracks => app.select_track(i32::MAX / 2),
        },
        KeyCode::Char(']') | KeyCode::PageDown => app.select_file(1),
        KeyCode::Char('[') | KeyCode::PageUp => app.select_file(-1),

        KeyCode::Char('d') => app.make_default(),
        KeyCode::Char('D') => app.clear_default(),
        KeyCode::Char('f') => app.toggle_flag(id::FLAG_FORCED),
        KeyCode::Char('e') => app.toggle_flag(id::FLAG_ENABLED),
        KeyCode::Char('h') => app.toggle_flag(id::FLAG_HEARING_IMPAIRED),
        KeyCode::Char('v') => app.toggle_flag(id::FLAG_VISUAL_IMPAIRED),
        KeyCode::Char('t') => app.toggle_flag(id::FLAG_TEXT_DESCRIPTIONS),
        KeyCode::Char('o') => app.toggle_flag(id::FLAG_ORIGINAL),
        KeyCode::Char('c') => app.toggle_flag(id::FLAG_COMMENTARY),
        KeyCode::Char('n') => app.start_input(InputTarget::Name),
        KeyCode::Char('l') => app.start_input(InputTarget::Language),

        KeyCode::Char('s') => app.save_current(),
        KeyCode::Char('S') => app.save_all(),
        KeyCode::Char('u') => app.revert_current(),
        _ => {}
    }
}
