//! Application state and the edits the UI can apply.

use std::path::{Path, PathBuf};

use crate::ebml::{Element, id};
use crate::edit::{self, SaveMode};
use crate::mkv::{MkvFile, Track, flag_insert_position};
use crate::scan::Scanner;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Files,
    Tracks,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputTarget {
    Name,
    Language,
}

pub struct InputState {
    pub target: InputTarget,
    pub prompt: String,
    pub value: String,
}

pub struct FileEntry {
    pub path: PathBuf,
    /// Loaded lazily on first selection.
    pub loaded: Option<Result<MkvFile, String>>,
    pub dirty: bool,
}

impl FileEntry {
    pub fn label(&self) -> String {
        self.path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }
}

pub struct App {
    pub files: Vec<FileEntry>,
    pub file_sel: usize,
    pub track_sel: usize,
    pub focus: Focus,
    pub status: String,
    pub status_error: bool,
    pub show_help: bool,
    pub input: Option<InputState>,
    pub confirm_quit: bool,
    pub backup: bool,
    pub should_quit: bool,
    /// Reads the rest of the directory in the background. Dropped once every
    /// file has arrived.
    scanner: Option<Scanner>,
}

impl App {
    pub fn new(paths: Vec<PathBuf>, backup: bool) -> App {
        App::with_scanner(paths, backup, true)
    }

    /// `background` reads the whole list on worker threads. Tests turn it off
    /// so that a run is deterministic.
    pub fn with_scanner(paths: Vec<PathBuf>, backup: bool, background: bool) -> App {
        let scanner = if background && paths.len() > 1 {
            Some(Scanner::start(&paths))
        } else {
            None
        };
        let files = paths
            .into_iter()
            .map(|p| FileEntry {
                path: p,
                loaded: None,
                dirty: false,
            })
            .collect();
        let mut app = App {
            files,
            file_sel: 0,
            track_sel: 0,
            focus: Focus::Tracks,
            status: String::new(),
            status_error: false,
            show_help: false,
            input: None,
            confirm_quit: false,
            backup,
            should_quit: false,
            scanner,
        };
        // The first file is read here rather than waited for, so the opening
        // frame already has something in it.
        app.ensure_loaded();
        app
    }

    /// Collects whatever the background scan has finished. Returns true when
    /// something arrived and the screen should be redrawn.
    pub fn poll_scan(&mut self) -> bool {
        let Some(mut scanner) = self.scanner.take() else {
            return false;
        };
        let files = &mut self.files;
        let changed = scanner.drain(|i, result| {
            if let Some(entry) = files.get_mut(i)
                && entry.loaded.is_none()
            {
                // Never clobber a file the cursor reached first, and in
                // particular never one with unsaved edits.
                entry.loaded = Some(result);
            }
        });
        if scanner.in_progress() {
            self.scanner = Some(scanner);
        }
        changed
    }

    /// How many files have been read so far, while that is still going on.
    pub fn scan_progress(&self) -> Option<(usize, usize)> {
        self.scanner.as_ref().map(|s| s.progress())
    }

    // -- selection ---------------------------------------------------------

    pub fn ensure_loaded(&mut self) {
        if let Some(entry) = self.files.get_mut(self.file_sel)
            && entry.loaded.is_none()
        {
            entry.loaded = Some(MkvFile::open(&entry.path));
        }
        let n = self.tracks().len();
        if self.track_sel >= n {
            self.track_sel = n.saturating_sub(1);
        }
    }

    pub fn current(&self) -> Option<&MkvFile> {
        self.files
            .get(self.file_sel)?
            .loaded
            .as_ref()?
            .as_ref()
            .ok()
    }

    pub fn current_error(&self) -> Option<&str> {
        match self.files.get(self.file_sel)?.loaded.as_ref()? {
            Err(e) => Some(e),
            Ok(_) => None,
        }
    }

    fn current_mut(&mut self) -> Option<&mut MkvFile> {
        self.files
            .get_mut(self.file_sel)?
            .loaded
            .as_mut()?
            .as_mut()
            .ok()
    }

    pub fn tracks(&self) -> Vec<Track> {
        self.current().map(|m| m.tracks_view()).unwrap_or_default()
    }

    pub fn selected_track(&self) -> Option<Track> {
        self.tracks().get(self.track_sel).cloned()
    }

    pub fn select_file(&mut self, delta: i32) {
        if self.files.is_empty() {
            return;
        }
        let n = self.files.len() as i32;
        let cur = self.file_sel as i32;
        let next = (cur + delta).clamp(0, n - 1);
        if next != cur {
            self.file_sel = next as usize;
            self.track_sel = 0;
            self.ensure_loaded();
        }
    }

    pub fn select_track(&mut self, delta: i32) {
        let n = self.tracks().len() as i32;
        if n == 0 {
            return;
        }
        let next = (self.track_sel as i32 + delta).clamp(0, n - 1);
        self.track_sel = next as usize;
    }

    pub fn info(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_error = false;
    }

    pub fn error(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_error = true;
    }

    fn mark_dirty(&mut self) {
        if let Some(e) = self.files.get_mut(self.file_sel) {
            e.dirty = true;
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.files
            .get(self.file_sel)
            .map(|e| e.dirty)
            .unwrap_or(false)
    }

    pub fn dirty_count(&self) -> usize {
        self.files.iter().filter(|e| e.dirty).count()
    }

    // -- edits -------------------------------------------------------------

    /// Sets an explicit flag value, but leaves a missing element missing when
    /// the requested value already matches the specification default. That
    /// keeps the Tracks element from growing for no reason.
    fn write_flag(entry: &mut Element, flag_id: u64, value: bool, spec_default: bool) {
        if value == spec_default && entry.find(flag_id).is_none() {
            return;
        }
        let at = flag_insert_position(entry);
        entry.set_uint(flag_id, value as u64, at);
    }

    fn with_selected<F: FnOnce(&mut Element)>(&mut self, f: F) -> bool {
        let idx = match self.selected_track() {
            Some(t) => t.index,
            None => return false,
        };
        let Some(mkv) = self.current_mut() else {
            return false;
        };
        let Some(children) = mkv.tracks.children_mut() else {
            return false;
        };
        let Some(entry) = children.get_mut(idx) else {
            return false;
        };
        f(entry);
        self.mark_dirty();
        true
    }

    /// Makes the selected track the default one of its type and clears the
    /// flag on every sibling of the same type.
    pub fn make_default(&mut self) {
        let Some(track) = self.selected_track() else {
            return;
        };
        let ttype = track.ttype;
        let idx = track.index;
        let Some(mkv) = self.current_mut() else {
            return;
        };
        let Some(children) = mkv.tracks.children_mut() else {
            return;
        };
        for (i, entry) in children.iter_mut().enumerate() {
            if entry.id != id::TRACK_ENTRY {
                continue;
            }
            if entry.get_uint(id::TRACK_TYPE).unwrap_or(0) != ttype {
                continue;
            }
            App::write_flag(entry, id::FLAG_DEFAULT, i == idx, true);
        }
        self.mark_dirty();
        let name = crate::ebml::track_type_name(ttype);
        self.info(format!(
            "track {} is now the default {name} track",
            track.number
        ));
    }

    pub fn clear_default(&mut self) {
        let Some(track) = self.selected_track() else {
            return;
        };
        self.with_selected(|e| App::write_flag(e, id::FLAG_DEFAULT, false, true));
        self.info(format!(
            "track {} is no longer a default track",
            track.number
        ));
    }

    pub fn toggle_flag(&mut self, flag_id: u64) {
        let Some(track) = self.selected_track() else {
            return;
        };
        let (cur, spec_default, label) = match flag_id {
            id::FLAG_FORCED => (track.forced.value, false, "forced"),
            id::FLAG_ENABLED => (track.enabled.value, true, "enabled"),
            id::FLAG_HEARING_IMPAIRED => (track.hearing_impaired.value, false, "hearing impaired"),
            id::FLAG_VISUAL_IMPAIRED => (track.visual_impaired.value, false, "visual impaired"),
            id::FLAG_TEXT_DESCRIPTIONS => {
                (track.text_descriptions.value, false, "text descriptions")
            }
            id::FLAG_ORIGINAL => (track.original.value, false, "original"),
            id::FLAG_COMMENTARY => (track.commentary.value, false, "commentary"),
            id::FLAG_DEFAULT => (track.default.value, true, "default"),
            _ => return,
        };
        let next = !cur;
        self.with_selected(|e| App::write_flag(e, flag_id, next, spec_default));
        let state = if next { "on" } else { "off" };
        self.info(format!("track {}: {label} {state}", track.number));
    }

    pub fn start_input(&mut self, target: InputTarget) {
        let Some(track) = self.selected_track() else {
            return;
        };
        let (prompt, value) = match target {
            InputTarget::Name => (
                "Track name (empty removes it)".to_string(),
                track.display_name(),
            ),
            InputTarget::Language => (
                "Language, ISO 639-2 such as eng or jpn".to_string(),
                track.language.clone(),
            ),
        };
        self.input = Some(InputState {
            target,
            prompt,
            value,
        });
    }

    pub fn commit_input(&mut self) {
        let Some(input) = self.input.take() else {
            return;
        };
        let value = input.value.trim().to_string();
        let Some(track) = self.selected_track() else {
            return;
        };
        match input.target {
            InputTarget::Name => {
                let v = value.clone();
                self.with_selected(move |e| {
                    if v.is_empty() {
                        e.remove(id::TRACK_NAME);
                    } else {
                        let at = flag_insert_position(e);
                        e.set_string(id::TRACK_NAME, &v, at);
                    }
                });
                if value.is_empty() {
                    self.info(format!("track {}: name removed", track.number));
                } else {
                    self.info(format!("track {}: name set to {value}", track.number));
                }
            }
            InputTarget::Language => {
                if value.is_empty() {
                    self.error("language cannot be empty");
                    return;
                }
                let v = value.clone();
                let had_bcp47 = track.language_bcp47.is_some();
                self.with_selected(move |e| {
                    let at = flag_insert_position(e);
                    e.set_string(id::LANGUAGE, &v, at);
                    // A BCP-47 tag overrides Language, so drop it rather than
                    // leave the two disagreeing.
                    e.remove(id::LANGUAGE_BCP47);
                });
                if had_bcp47 {
                    self.info(format!(
                        "track {}: language set to {value}, BCP-47 tag removed",
                        track.number
                    ));
                } else {
                    self.info(format!("track {}: language set to {value}", track.number));
                }
            }
        }
    }

    // -- saving ------------------------------------------------------------

    pub fn save_current(&mut self) {
        if !self.is_dirty() {
            self.info("no changes to save");
            return;
        }
        let backup = self.backup;
        let Some(mkv) = self.current() else { return };
        let path = mkv.path.clone();
        match edit::save(mkv, backup) {
            Ok(report) => {
                let extra = if report.mode == SaveMode::Rewrite {
                    " (file rewritten)"
                } else {
                    ""
                };
                self.reload(self.file_sel);
                if let Some(e) = self.files.get_mut(self.file_sel) {
                    e.dirty = false;
                }
                self.info(format!("{}: {}{extra}", file_label(&path), report.message));
            }
            Err(e) => self.error(format!("{}: {e}", file_label(&path))),
        }
    }

    pub fn save_all(&mut self) {
        let indices: Vec<usize> = (0..self.files.len())
            .filter(|i| self.files[*i].dirty)
            .collect();
        if indices.is_empty() {
            self.info("no changes to save");
            return;
        }
        let backup = self.backup;
        let mut saved = 0usize;
        let mut failures: Vec<String> = Vec::new();
        for i in indices {
            let Some(Ok(mkv)) = self.files[i].loaded.as_ref() else {
                continue;
            };
            match edit::save(mkv, backup) {
                Ok(_) => {
                    saved += 1;
                    self.reload(i);
                    self.files[i].dirty = false;
                }
                Err(e) => failures.push(format!("{}: {e}", self.files[i].label())),
            }
        }
        if failures.is_empty() {
            self.info(format!("saved {saved} file(s)"));
        } else {
            self.error(format!(
                "saved {saved}, failed {}: {}",
                failures.len(),
                failures[0]
            ));
        }
    }

    fn reload(&mut self, index: usize) {
        if let Some(e) = self.files.get_mut(index) {
            e.loaded = Some(MkvFile::open(&e.path));
        }
    }

    pub fn revert_current(&mut self) {
        if !self.is_dirty() {
            self.info("no changes to revert");
            return;
        }
        self.reload(self.file_sel);
        if let Some(e) = self.files.get_mut(self.file_sel) {
            e.dirty = false;
        }
        self.ensure_loaded();
        self.info("reloaded from disk, changes discarded");
    }
}

pub fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}
