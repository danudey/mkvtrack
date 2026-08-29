//! Rendering.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
};

use crate::app::{App, Focus};
use crate::ebml::track_type;
use crate::mkv::{Flag, Track, language_name};

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(6),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    let body = rows[0];
    let show_files = app.files.len() > 1;
    let panes = if show_files {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(34), Constraint::Min(40)])
            .split(body)
    } else {
        Layout::default()
            .constraints([Constraint::Min(0)])
            .split(body)
    };

    if show_files {
        draw_files(f, app, panes[0]);
    }
    let right = panes[if show_files { 1 } else { 0 }];
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(12)])
        .split(right);
    draw_tracks(f, app, split[0]);
    draw_details(f, app, split[1]);

    draw_status(f, app, rows[1]);
    draw_keys(f, app, rows[2]);

    if app.show_help {
        draw_help(f, area);
    }
    if app.confirm_quit {
        draw_confirm(f, app, area);
    }
}

fn draw_files(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .files
        .iter()
        .map(|e| {
            let mark = if e.dirty { "*" } else { " " };
            let broken = matches!(e.loaded, Some(Err(_)));
            let style = if broken {
                Style::default().fg(Color::Red)
            } else if e.dirty {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(mark, Style::default().fg(Color::Yellow)),
                Span::styled(format!(" {}", e.label()), style),
            ]))
        })
        .collect();

    let title = match app.scan_progress() {
        Some((done, total)) => format!(" Files ({done}/{total} read) "),
        None => format!(" Files ({}) ", app.files.len()),
    };
    let list = List::new(items)
        .block(block(&title, app.focus == Focus::Files))
        .highlight_style(selection_style(app.focus == Focus::Files));
    let mut state = ListState::default();
    state.select(Some(app.file_sel));
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_tracks(f: &mut Frame, app: &App, area: Rect) {
    let title = match app.current() {
        Some(m) => format!(" Tracks - {} ", crate::app::file_label(&m.path)),
        None => " Tracks ".to_string(),
    };

    if let Some(err) = app.current_error() {
        let p = Paragraph::new(vec![
            Line::from(Span::styled(
                "This file could not be read:",
                Style::default().fg(Color::Red),
            )),
            Line::from(""),
            Line::from(err.to_string()),
        ])
        .wrap(Wrap { trim: true })
        .block(block(&title, app.focus == Focus::Tracks));
        f.render_widget(p, area);
        return;
    }

    let tracks = app.tracks();
    let header = Row::new(vec![
        "ID", "Type", "Codec", "Lang", "Name", "Layout", "Flags",
    ])
    .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = tracks
        .iter()
        .map(|t| {
            let lang = t.effective_language();
            let style = match t.ttype {
                track_type::AUDIO => Style::default().fg(Color::Green),
                track_type::SUBTITLE => Style::default().fg(Color::Magenta),
                track_type::VIDEO => Style::default().fg(Color::Blue),
                _ => Style::default().fg(DIM),
            };
            Row::new(vec![
                t.number.to_string(),
                t.type_name().to_string(),
                codec_label(&t.codec_id),
                lang,
                t.display_name(),
                t.channel_layout(),
                t.flag_summary(),
            ])
            .style(style)
        })
        .collect();

    let widths = [
        Constraint::Length(3),
        Constraint::Length(8),
        Constraint::Length(14),
        Constraint::Length(6),
        Constraint::Min(16),
        Constraint::Length(7),
        Constraint::Length(16),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(block(&title, app.focus == Focus::Tracks))
        .row_highlight_style(selection_style(app.focus == Focus::Tracks))
        .highlight_symbol("> ");
    let mut state = TableState::default();
    state.select(if tracks.is_empty() {
        None
    } else {
        Some(app.track_sel)
    });
    f.render_stateful_widget(table, area, &mut state);
}

/// `show_implied` marks values the file does not state, which matters for the
/// flags a player acts on.
fn flag_span(label: &str, flag: Flag, show_implied: bool) -> Vec<Span<'static>> {
    let value = if flag.value { "yes" } else { "no" };
    let colour = if flag.value { Color::Yellow } else { DIM };
    let suffix = if flag.explicit || !show_implied {
        ""
    } else {
        " (implied)"
    };
    vec![
        Span::styled(format!("{label}="), Style::default().fg(DIM)),
        Span::styled(value.to_string(), Style::default().fg(colour)),
        Span::styled(suffix.to_string(), Style::default().fg(DIM)),
        Span::raw("  "),
    ]
}

fn draw_details(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    if let Some(mkv) = app.current() {
        let mut head: Vec<Span> = Vec::new();
        if let Some(t) = &mkv.info.title {
            head.push(Span::styled("Title: ", Style::default().fg(DIM)));
            head.push(Span::raw(t.clone()));
            head.push(Span::raw("   "));
        }
        if let Some(d) = mkv.info.duration_secs {
            head.push(Span::styled("Duration: ", Style::default().fg(DIM)));
            head.push(Span::raw(fmt_duration(d)));
            head.push(Span::raw("   "));
        }
        if let Some(w) = &mkv.info.writing_app {
            head.push(Span::styled("Written by: ", Style::default().fg(DIM)));
            head.push(Span::raw(w.clone()));
        }
        if !head.is_empty() {
            lines.push(Line::from(head));
        }
    }

    if let Some(t) = app.selected_track() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("Track {} ", t.number),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(t.type_name().to_string(), Style::default().fg(ACCENT)),
            Span::raw("  "),
            Span::raw(codec_label(&t.codec_id)),
            Span::styled(format!("  [{}]", t.codec_id), Style::default().fg(DIM)),
        ]));

        let code = t.language.clone();
        let mut lang_line = vec![
            Span::styled("Language: ", Style::default().fg(DIM)),
            Span::raw(match language_name(&code) {
                Some(n) => format!("{code} ({n})"),
                None => code.clone(),
            }),
        ];
        if !t.language_explicit {
            lang_line.push(Span::styled(" (implied)", Style::default().fg(DIM)));
        }
        if let Some(b) = &t.language_bcp47 {
            lang_line.push(Span::styled("   BCP-47: ", Style::default().fg(DIM)));
            lang_line.push(Span::styled(b.clone(), Style::default().fg(Color::Yellow)));
            lang_line.push(Span::styled(
                " (overrides Language)",
                Style::default().fg(DIM),
            ));
        }
        lines.push(Line::from(lang_line));

        lines.push(Line::from(vec![
            Span::styled("Name: ", Style::default().fg(DIM)),
            Span::raw(t.name.clone().unwrap_or_else(|| "-".into())),
        ]));

        let mut flags: Vec<Span> = Vec::new();
        flags.extend(flag_span("default", t.default, true));
        flags.extend(flag_span("forced", t.forced, true));
        flags.extend(flag_span("enabled", t.enabled, true));
        lines.push(Line::from(flags));

        let mut flags2: Vec<Span> = Vec::new();
        flags2.extend(flag_span("hearing impaired", t.hearing_impaired, false));
        flags2.extend(flag_span("visual impaired", t.visual_impaired, false));
        flags2.extend(flag_span("text descriptions", t.text_descriptions, false));
        flags2.extend(flag_span("original", t.original, false));
        flags2.extend(flag_span("commentary", t.commentary, false));
        lines.push(Line::from(flags2));

        if let Some(a) = &t.audio {
            let mut parts: Vec<String> = Vec::new();
            if let Some(c) = a.channels {
                parts.push(format!("{c} channels ({})", t.channel_layout()));
            }
            if let Some(hz) = a.sampling_frequency.filter(|h| *h > 0.0) {
                parts.push(format!("{} Hz", hz.round() as u64));
            }
            if let Some(hz) = a.output_sampling_frequency.filter(|h| *h > 0.0) {
                parts.push(format!("output {} Hz", hz.round() as u64));
            }
            if let Some(b) = a.bit_depth {
                parts.push(format!("{b} bit"));
            }
            if let Some(d) = t.codec_delay.filter(|d| *d > 0) {
                parts.push(format!("codec delay {} ms", d / 1_000_000));
            }
            lines.push(Line::from(vec![
                Span::styled("Audio: ", Style::default().fg(DIM)),
                Span::raw(parts.join(", ")),
            ]));
        }
        if let Some(v) = &t.video {
            let mut parts: Vec<String> = Vec::new();
            if let (Some(w), Some(h)) = (v.pixel_width, v.pixel_height) {
                parts.push(format!("{w}x{h}"));
            }
            if let (Some(w), Some(h)) = (v.display_width, v.display_height) {
                parts.push(format!("display {w}x{h}"));
            }
            if let Some(d) = t.default_duration.filter(|d| *d > 0) {
                parts.push(format!("{:.3} fps", 1_000_000_000.0 / d as f64));
            }
            lines.push(Line::from(vec![
                Span::styled("Video: ", Style::default().fg(DIM)),
                Span::raw(parts.join(", ")),
            ]));
        }

        let mut extra: Vec<String> = vec![format!("UID {}", t.uid)];
        if t.codec_private_len > 0 {
            extra.push(format!("codec private {} bytes", t.codec_private_len));
        }
        if t.compressed {
            extra.push("content encoding (compressed)".into());
        }
        if let Some(n) = &t.codec_name {
            extra.push(n.clone());
        }
        lines.push(Line::from(Span::styled(
            extra.join("  |  "),
            Style::default().fg(DIM),
        )));
    } else if app.current().is_some() {
        lines.push(Line::from("no tracks"));
    }

    let p = Paragraph::new(lines)
        .block(block(" Details ", false))
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    if let Some(input) = &app.input {
        let p = Paragraph::new(Line::from(vec![
            Span::styled(format!("{}: ", input.prompt), Style::default().fg(ACCENT)),
            Span::raw(input.value.clone()),
            Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
        ]));
        f.render_widget(p, area);
        return;
    }
    let style = if app.status_error {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Green)
    };
    let dirty = app.dirty_count();
    let suffix = if dirty > 0 {
        format!("  [{dirty} file(s) with unsaved changes]")
    } else {
        String::new()
    };
    let p = Paragraph::new(Line::from(vec![
        Span::styled(app.status.clone(), style),
        Span::styled(suffix, Style::default().fg(Color::Yellow)),
    ]));
    f.render_widget(p, area);
}

fn draw_keys(f: &mut Frame, app: &App, area: Rect) {
    let text = if app.input.is_some() {
        "Enter confirm   Esc cancel"
    } else {
        "d default  D clear  f forced  e enabled  n name  l language  s save  S save all  u revert  ? help  q quit"
    };
    let p = Paragraph::new(Span::styled(text, Style::default().fg(DIM)));
    f.render_widget(p, area);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(Span::styled(
            "Navigation",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from("  up/down, j/k     move within the focused pane"),
        Line::from("  Tab              switch between files and tracks"),
        Line::from("  [ / ]            previous / next file"),
        Line::from(""),
        Line::from(Span::styled(
            "Flags column",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from("  D default  F forced  HI/VI hearing or visual impaired  TD text descriptions"),
        Line::from("  Orig original  Com commentary  off disabled"),
        Line::from("  Lower case means the file does not say so; it is the specification default."),
        Line::from(""),
        Line::from(Span::styled(
            "Track flags",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from("  d   make this the default track of its type, clearing its siblings"),
        Line::from("  D   clear the default flag on this track"),
        Line::from("  f   toggle forced        e   toggle enabled"),
        Line::from("  h   hearing impaired     v   visual impaired"),
        Line::from("  t   text descriptions    o   original language"),
        Line::from("  c   commentary"),
        Line::from(""),
        Line::from(Span::styled(
            "Track properties",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from("  n   edit the track name (empty removes it)"),
        Line::from("  l   edit the language, e.g. eng, jpn, und"),
        Line::from(""),
        Line::from(Span::styled(
            "Files",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from("  s   save the current file      S   save every changed file"),
        Line::from("  u   discard changes to the current file"),
        Line::from("  q   quit          ?/F1  close this help"),
        Line::from(""),
        Line::from(Span::styled(
            "Changes are written to the Tracks element only. When it still fits, the write is a \
             few bytes in place; otherwise the file is rewritten and the seek index is corrected.",
            Style::default().fg(DIM),
        )),
    ];
    let popup = centered(78, 26, area);
    f.render_widget(Clear, popup);
    let p = Paragraph::new(lines)
        .block(block(" Help ", true))
        .wrap(Wrap { trim: false });
    f.render_widget(p, popup);
}

fn draw_confirm(f: &mut Frame, app: &App, area: Rect) {
    let popup = centered(60, 7, area);
    f.render_widget(Clear, popup);
    let lines = vec![
        Line::from(format!(
            "{} file(s) have unsaved changes.",
            app.dirty_count()
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("s", Style::default().fg(ACCENT)),
            Span::raw(" save all and quit    "),
            Span::styled("q", Style::default().fg(ACCENT)),
            Span::raw(" quit anyway    "),
            Span::styled("Esc", Style::default().fg(ACCENT)),
            Span::raw(" cancel"),
        ]),
    ];
    let p = Paragraph::new(lines)
        .block(block(" Quit ", true))
        .alignment(Alignment::Center);
    f.render_widget(p, popup);
}

fn block(title: &str, focused: bool) -> Block<'_> {
    let style = if focused {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(DIM)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(Span::styled(
            title.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ))
}

fn selection_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::REVERSED)
    }
}

fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn fmt_duration(secs: f64) -> String {
    let total = secs.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Friendly name for the codecs that turn up in Matroska files.
pub fn codec_label(codec_id: &str) -> String {
    let name = match codec_id {
        "A_AAC" => "AAC",
        "A_AC3" => "AC-3",
        "A_EAC3" => "E-AC-3",
        "A_DTS" => "DTS",
        "A_TRUEHD" => "TrueHD",
        "A_FLAC" => "FLAC",
        "A_OPUS" => "Opus",
        "A_VORBIS" => "Vorbis",
        "A_MPEG/L3" => "MP3",
        "A_MPEG/L2" => "MP2",
        "A_ALAC" => "ALAC",
        "S_TEXT/UTF8" => "SubRip",
        "S_TEXT/ASS" => "ASS",
        "S_TEXT/SSA" => "SSA",
        "S_TEXT/WEBVTT" => "WebVTT",
        "S_HDMV/PGS" => "PGS",
        "S_HDMV/TEXTST" => "TextST",
        "S_VOBSUB" => "VobSub",
        "S_DVBSUB" => "DVB sub",
        "S_KATE" => "Kate",
        "V_MPEG4/ISO/AVC" => "H.264",
        "V_MPEGH/ISO/HEVC" => "H.265",
        "V_AV1" => "AV1",
        "V_VP9" => "VP9",
        "V_VP8" => "VP8",
        "V_MPEG2" => "MPEG-2",
        "V_MS/VFW/FOURCC" => "VfW",
        other => {
            if let Some(rest) = other.strip_prefix("A_PCM/") {
                return format!("PCM {}", rest.to_ascii_lowercase());
            }
            return other.to_string();
        }
    };
    name.to_string()
}

/// One line summary of a track, used by the non-interactive listing.
pub fn plain_line(t: &Track) -> String {
    let lang = t.effective_language();
    let mut s = format!(
        "  {:>3}  {:<9} {:<14} {:<6}",
        t.number,
        t.type_name(),
        codec_label(&t.codec_id),
        lang
    );
    let name = t.display_name();
    if !name.is_empty() {
        s.push_str(&format!(" {name}"));
    }
    let flags = t.flag_summary();
    if !flags.is_empty() {
        s.push_str(&format!("  [{flags}]"));
    }
    s
}
