//! Interactive L3 storybook viewer.
//!
//! Reads the blessed `.snap` files under `crates/tui/tests/storybook/snapshots/`
//! and renders each story's frame with its styles sidecar applied. No story
//! re-execution — the snapshot files already contain everything the user
//! would see.
//!
//! Keys: `j`/`k` or `↓`/`↑` navigate, `g`/`G` jump to top/bottom,
//! `q` or `Esc` quits.
//!
//! Run from the workspace root:
//!     cargo run -p tui --example stories

use std::collections::HashMap;
use std::io::{stdout, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::{
    Attribute, Color as CtColor, Print, ResetColor, SetAttribute, SetBackgroundColor,
    SetForegroundColor,
};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{ExecutableCommand, QueueableCommand};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct StyleRun {
    fg: Option<CtColor>,
    bg: Option<CtColor>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    crossedout: bool,
}

impl StyleRun {
    fn is_default(&self) -> bool {
        self == &StyleRun::default()
    }
}

/// (row, col, len, style)
type StyleSpan = (u16, u16, u16, StyleRun);

#[derive(Clone, Debug)]
struct Story {
    /// Group name (e.g. `"layout"`).
    group: String,
    /// Stem after the `::` (e.g. `"vbox_three_panes"` or
    /// `"theme_swap_repaints_without_buffer_edit.step-1"`).
    name: String,
    /// Full snapshot id minus `.snap` suffix; used as filename root.
    full_id: String,
    /// Path to the `.snap` text file.
    snap_path: PathBuf,
    /// Path to the corresponding `.styles.snap` file (may not exist
    /// if no spans were emitted).
    styles_path: PathBuf,
}

fn snapshot_dir() -> PathBuf {
    // `cargo run --example` runs from the package directory.
    let cwd = std::env::current_dir().expect("cwd");
    let candidate = cwd.join("tests").join("storybook").join("snapshots");
    if candidate.is_dir() {
        return candidate;
    }
    // Fallback: workspace-root invocation.
    cwd.join("crates")
        .join("tui")
        .join("tests")
        .join("storybook")
        .join("snapshots")
}

fn enumerate_stories() -> Vec<Story> {
    let dir = snapshot_dir();
    let mut entries: Vec<Story> = Vec::new();
    let read = match std::fs::read_dir(&dir) {
        Ok(it) => it,
        Err(e) => {
            eprintln!("could not read {}: {e}", dir.display());
            std::process::exit(1);
        }
    };
    for entry in read.flatten() {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // Skip styles sidecars and any `.snap.new` drift files.
        if !file_name.ends_with(".snap") {
            continue;
        }
        if file_name.ends_with(".styles.snap") {
            continue;
        }
        let id = file_name.trim_end_matches(".snap").to_string();
        let (group, name) = match id.split_once("::") {
            Some((g, n)) => (g.to_string(), n.to_string()),
            None => (String::from("misc"), id.clone()),
        };
        let styles_path = dir.join(format!("{id}.styles.snap"));
        entries.push(Story {
            group,
            name,
            full_id: id.clone(),
            snap_path: path.clone(),
            styles_path,
        });
    }
    entries.sort_by(|a, b| {
        (a.group.as_str(), a.name.as_str()).cmp(&(b.group.as_str(), b.name.as_str()))
    });
    entries
}

/// Strip the insta YAML header from a `.snap` body and return the
/// content lines. The format is:
///
///     ---
///     source: …
///     expression: …
///     ---
///     <body>
fn read_snap_body(path: &Path) -> std::io::Result<Vec<String>> {
    let raw = std::fs::read_to_string(path)?;
    let mut iter = raw.lines();
    // First line should be `---`.
    if iter.next() != Some("---") {
        // No header: return the whole file.
        return Ok(raw.lines().map(|l| l.to_string()).collect());
    }
    // Skip header until the second `---`.
    for line in iter.by_ref() {
        if line == "---" {
            break;
        }
    }
    Ok(iter.map(|l| l.to_string()).collect())
}

/// Parse one styles-sidecar body. Each line has the shape:
///     row col len fg=… bg=… attrs=bold|italic
/// `row col len` are right-aligned ints. Empty lines collapse out.
fn parse_styles(body: &[String]) -> Vec<StyleSpan> {
    let mut out = Vec::new();
    for line in body {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(row) = parts.next().and_then(|s| s.parse::<u16>().ok()) else {
            continue;
        };
        let Some(col) = parts.next().and_then(|s| s.parse::<u16>().ok()) else {
            continue;
        };
        let Some(len) = parts.next().and_then(|s| s.parse::<u16>().ok()) else {
            continue;
        };
        let mut style = StyleRun::default();
        for kv in parts {
            if let Some(rest) = kv.strip_prefix("fg=") {
                style.fg = parse_color(rest);
            } else if let Some(rest) = kv.strip_prefix("bg=") {
                style.bg = parse_color(rest);
            } else if let Some(rest) = kv.strip_prefix("attrs=") {
                for attr in rest.split('|') {
                    match attr {
                        "bold" => style.bold = true,
                        "dim" => style.dim = true,
                        "italic" => style.italic = true,
                        "underline" => style.underline = true,
                        "crossedout" => style.crossedout = true,
                        _ => {}
                    }
                }
            }
        }
        out.push((row, col, len, style));
    }
    out
}

/// Parse the `Debug` output of `smelt_core::style::Color` back into
/// crossterm's `Color`. Names match 1-to-1 except `Rgb { r, g, b }`
/// and `AnsiValue(n)`.
fn parse_color(s: &str) -> Option<CtColor> {
    if let Some(rest) = s.strip_prefix("Rgb { ") {
        // Form: `Rgb { r: 12, g: 34, b: 56 }` — split on commas.
        let inner = rest.trim_end_matches(" }");
        let mut r = None;
        let mut g = None;
        let mut b = None;
        for kv in inner.split(',') {
            let kv = kv.trim();
            if let Some(v) = kv.strip_prefix("r:") {
                r = v.trim().parse().ok();
            } else if let Some(v) = kv.strip_prefix("g:") {
                g = v.trim().parse().ok();
            } else if let Some(v) = kv.strip_prefix("b:") {
                b = v.trim().parse().ok();
            }
        }
        return match (r, g, b) {
            (Some(r), Some(g), Some(b)) => Some(CtColor::Rgb { r, g, b }),
            _ => None,
        };
    }
    if let Some(rest) = s.strip_prefix("AnsiValue(") {
        let n: u8 = rest.trim_end_matches(')').parse().ok()?;
        return Some(CtColor::AnsiValue(n));
    }
    Some(match s {
        "Reset" => CtColor::Reset,
        "Black" => CtColor::Black,
        "DarkGrey" | "DarkGray" => CtColor::DarkGrey,
        "Red" => CtColor::Red,
        "DarkRed" => CtColor::DarkRed,
        "Green" => CtColor::Green,
        "DarkGreen" => CtColor::DarkGreen,
        "Yellow" => CtColor::Yellow,
        "DarkYellow" => CtColor::DarkYellow,
        "Blue" => CtColor::Blue,
        "DarkBlue" => CtColor::DarkBlue,
        "Magenta" => CtColor::Magenta,
        "DarkMagenta" => CtColor::DarkMagenta,
        "Cyan" => CtColor::Cyan,
        "DarkCyan" => CtColor::DarkCyan,
        "White" => CtColor::White,
        "Grey" | "Gray" => CtColor::Grey,
        _ => return None,
    })
}

/// Build a per-cell style table for a frame. `rows.len()` rows, each
/// padded to `width` cells. Anything not covered by a span stays
/// default.
fn cell_styles(rows: &[String], spans: &[StyleSpan], width: usize) -> Vec<Vec<StyleRun>> {
    let mut out: Vec<Vec<StyleRun>> = (0..rows.len())
        .map(|_| vec![StyleRun::default(); width])
        .collect();
    for (r, c, len, style) in spans {
        let r = *r as usize;
        if r >= out.len() {
            continue;
        }
        let start = *c as usize;
        let end = (*c as usize + *len as usize).min(width);
        for cell in &mut out[r][start..end] {
            *cell = style.clone();
        }
    }
    out
}

fn frame_width(rows: &[String], spans: &[StyleSpan]) -> usize {
    let from_text = rows.iter().map(|r| r.chars().count()).max().unwrap_or(0);
    let from_styles = spans
        .iter()
        .map(|(_, c, l, _)| *c as usize + *l as usize)
        .max()
        .unwrap_or(0);
    from_text.max(from_styles)
}

// ── List-pane rendering ───────────────────────────────────────────

fn group_counts(stories: &[Story]) -> Vec<(String, usize)> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for s in stories {
        *counts.entry(s.group.clone()).or_insert(0) += 1;
    }
    let mut pairs: Vec<_> = counts.into_iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs
}

// ── App state + main loop ─────────────────────────────────────────

struct App {
    stories: Vec<Story>,
    selected: usize,
    list_scroll: usize,
}

impl App {
    fn new(stories: Vec<Story>) -> Self {
        Self {
            stories,
            selected: 0,
            list_scroll: 0,
        }
    }

    fn move_selection(&mut self, delta: i32, list_height: usize) {
        if self.stories.is_empty() {
            return;
        }
        let len = self.stories.len() as i32;
        let mut idx = self.selected as i32 + delta;
        if idx < 0 {
            idx = 0;
        } else if idx >= len {
            idx = len - 1;
        }
        self.selected = idx as usize;
        // Keep the selection in view.
        if self.selected < self.list_scroll {
            self.list_scroll = self.selected;
        }
        if self.selected >= self.list_scroll + list_height {
            self.list_scroll = self.selected + 1 - list_height;
        }
    }

    fn jump(&mut self, top: bool, list_height: usize) {
        if self.stories.is_empty() {
            return;
        }
        if top {
            self.selected = 0;
            self.list_scroll = 0;
        } else {
            self.selected = self.stories.len() - 1;
            self.list_scroll = self.selected.saturating_sub(list_height.saturating_sub(1));
        }
    }
}

fn render(app: &App, term_w: u16, term_h: u16) -> std::io::Result<()> {
    let mut out = stdout();
    out.queue(Clear(ClearType::All))?;

    // Layout: left list, gap=1, right preview. List width = 32 cells.
    let list_w: u16 = 32;
    let gap: u16 = 1;
    let preview_left = list_w + gap;
    let preview_w = term_w.saturating_sub(preview_left);
    let header_h: u16 = 2;
    let footer_h: u16 = 2;
    let content_top: u16 = header_h;
    let content_h = term_h.saturating_sub(header_h + footer_h);

    // ── Header ────────────────────────────────────────────────────
    out.queue(cursor::MoveTo(0, 0))?;
    out.queue(SetAttribute(Attribute::Bold))?;
    out.queue(Print(format!(
        "smelt storybook — {} stories across {} groups",
        app.stories.len(),
        group_counts(&app.stories).len()
    )))?;
    out.queue(SetAttribute(Attribute::Reset))?;

    // ── List pane ─────────────────────────────────────────────────
    let mut last_group: Option<&str> = None;
    for (offset, (i, story)) in app
        .stories
        .iter()
        .enumerate()
        .skip(app.list_scroll)
        .take(content_h as usize)
        .enumerate()
    {
        let row = content_top + offset as u16;
        if row >= content_top + content_h {
            break;
        }
        out.queue(cursor::MoveTo(0, row))?;
        // Group break.
        let g_changed = last_group != Some(story.group.as_str());
        last_group = Some(story.group.as_str());
        let selected = i == app.selected;
        if selected {
            out.queue(SetAttribute(Attribute::Reverse))?;
        } else if g_changed {
            out.queue(SetAttribute(Attribute::Dim))?;
        }
        let mut label = if g_changed {
            format!("▸ {} :: {}", story.group, story.name)
        } else {
            format!("    {}", story.name)
        };
        // Truncate to list width.
        if label.chars().count() > list_w as usize {
            let truncated: String = label.chars().take(list_w as usize - 1).collect();
            label = format!("{truncated}…");
        }
        // Pad to fill the row so background reverse covers the whole bar.
        let pad = (list_w as usize).saturating_sub(label.chars().count());
        out.queue(Print(label))?;
        if pad > 0 {
            out.queue(Print(" ".repeat(pad)))?;
        }
        out.queue(SetAttribute(Attribute::Reset))?;
    }

    // ── Vertical separator ────────────────────────────────────────
    for r in content_top..(content_top + content_h) {
        out.queue(cursor::MoveTo(list_w, r))?;
        out.queue(SetAttribute(Attribute::Dim))?;
        out.queue(Print("│"))?;
        out.queue(SetAttribute(Attribute::Reset))?;
    }

    // ── Preview pane ──────────────────────────────────────────────
    if let Some(story) = app.stories.get(app.selected) {
        let rows = read_snap_body(&story.snap_path).unwrap_or_default();
        let style_rows = if story.styles_path.exists() {
            parse_styles(&read_snap_body(&story.styles_path).unwrap_or_default())
        } else {
            Vec::new()
        };
        let width = frame_width(&rows, &style_rows);
        let cells = cell_styles(&rows, &style_rows, width);

        // Title.
        out.queue(cursor::MoveTo(preview_left, content_top))?;
        out.queue(SetAttribute(Attribute::Bold))?;
        out.queue(Print(&story.full_id))?;
        out.queue(SetAttribute(Attribute::Reset))?;
        out.queue(cursor::MoveTo(preview_left, content_top + 1))?;
        out.queue(SetAttribute(Attribute::Dim))?;
        out.queue(Print(format!("frame: {} × {}", width, rows.len())))?;
        out.queue(SetAttribute(Attribute::Reset))?;

        // Frame body, framed by a thin rule above + below.
        let body_top = content_top + 3;
        let frame_max_w = preview_w.saturating_sub(2) as usize;
        let render_w = width.min(frame_max_w);
        // Top rule.
        out.queue(cursor::MoveTo(preview_left, body_top.saturating_sub(1)))?;
        out.queue(SetAttribute(Attribute::Dim))?;
        out.queue(Print(format!("┌{}┐", "─".repeat(render_w))))?;
        out.queue(SetAttribute(Attribute::Reset))?;

        let max_rows = (content_h as usize).saturating_sub(5);
        for (r, line) in rows.iter().enumerate().take(max_rows) {
            let frame_row = body_top + r as u16;
            if frame_row >= content_top + content_h - 1 {
                break;
            }
            out.queue(cursor::MoveTo(preview_left, frame_row))?;
            out.queue(SetAttribute(Attribute::Dim))?;
            out.queue(Print("│"))?;
            out.queue(SetAttribute(Attribute::Reset))?;
            // The snap stores 1 char per visual column — a width-2
            // glyph occupies its column plus a trailing placeholder
            // space. The terminal, however, advances by the glyph's
            // own width when we print it, so emitting both columns
            // double-counts the wide cell and pushes the trailing
            // border right. Skip the placeholder when the preceding
            // glyph is wide.
            use unicode_width::UnicodeWidthChar;
            let chars: Vec<char> = line.chars().collect();
            let row_styles = cells.get(r);
            let mut col = 0usize;
            let mut emitted: u16 = 0;
            while col < render_w {
                let ch = chars.get(col).copied().unwrap_or(' ');
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
                if emitted as usize + cw > render_w {
                    break;
                }
                let style = row_styles
                    .and_then(|s| s.get(col).cloned())
                    .unwrap_or_default();
                apply_style(&mut out, &style)?;
                let mut buf = [0u8; 4];
                out.queue(Print(ch.encode_utf8(&mut buf).to_string()))?;
                out.queue(ResetColor)?;
                out.queue(SetAttribute(Attribute::Reset))?;
                col += cw;
                emitted += cw as u16;
            }
            // Pad with spaces if the row had fewer visual cells than
            // `render_w` — keeps the trailing `│` aligned.
            while emitted < render_w as u16 {
                out.queue(Print(" "))?;
                emitted += 1;
            }
            out.queue(SetAttribute(Attribute::Dim))?;
            out.queue(Print("│"))?;
            out.queue(SetAttribute(Attribute::Reset))?;
        }
        // Bottom rule.
        let bottom_row = body_top + (rows.len().min(max_rows) as u16);
        if bottom_row < content_top + content_h {
            out.queue(cursor::MoveTo(preview_left, bottom_row))?;
            out.queue(SetAttribute(Attribute::Dim))?;
            out.queue(Print(format!("└{}┘", "─".repeat(render_w))))?;
            out.queue(SetAttribute(Attribute::Reset))?;
        }
    }

    // ── Footer ────────────────────────────────────────────────────
    out.queue(cursor::MoveTo(0, term_h.saturating_sub(1)))?;
    out.queue(SetAttribute(Attribute::Dim))?;
    out.queue(Print("j/k or ↓/↑ navigate · g/G top/bottom · q/Esc quit"))?;
    out.queue(SetAttribute(Attribute::Reset))?;

    out.flush()?;
    Ok(())
}

fn apply_style<W: Write>(w: &mut W, s: &StyleRun) -> std::io::Result<()> {
    if s.is_default() {
        return Ok(());
    }
    if let Some(fg) = s.fg {
        w.queue(SetForegroundColor(fg))?;
    }
    if let Some(bg) = s.bg {
        w.queue(SetBackgroundColor(bg))?;
    }
    if s.bold {
        w.queue(SetAttribute(Attribute::Bold))?;
    }
    if s.dim {
        w.queue(SetAttribute(Attribute::Dim))?;
    }
    if s.italic {
        w.queue(SetAttribute(Attribute::Italic))?;
    }
    if s.underline {
        w.queue(SetAttribute(Attribute::Underlined))?;
    }
    if s.crossedout {
        w.queue(SetAttribute(Attribute::CrossedOut))?;
    }
    Ok(())
}

fn run() -> std::io::Result<()> {
    let stories = enumerate_stories();
    if stories.is_empty() {
        eprintln!(
            "no stories found under {} — run `cargo nextest run -p tui --test storybook_main` first.",
            snapshot_dir().display()
        );
        std::process::exit(1);
    }

    terminal::enable_raw_mode()?;
    let mut out = stdout();
    out.execute(EnterAlternateScreen)?;
    out.execute(cursor::Hide)?;

    let mut app = App::new(stories);
    let mut size = terminal::size()?;
    let result = (|| -> std::io::Result<()> {
        loop {
            let list_height = (size.1 as usize).saturating_sub(4);
            render(&app, size.0, size.1)?;
            if event::poll(Duration::from_millis(200))? {
                match event::read()? {
                    Event::Key(KeyEvent {
                        code, modifiers, ..
                    }) => match code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => break,
                        KeyCode::Char('j') | KeyCode::Down => app.move_selection(1, list_height),
                        KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1, list_height),
                        KeyCode::PageDown => {
                            app.move_selection((list_height as i32) - 2, list_height)
                        }
                        KeyCode::PageUp => {
                            app.move_selection(-((list_height as i32) - 2), list_height)
                        }
                        KeyCode::Char('g') => app.jump(true, list_height),
                        KeyCode::Char('G') => app.jump(false, list_height),
                        KeyCode::Home => app.jump(true, list_height),
                        KeyCode::End => app.jump(false, list_height),
                        _ => {}
                    },
                    Event::Resize(w, h) => size = (w, h),
                    _ => {}
                }
            }
        }
        Ok(())
    })();

    out.execute(cursor::Show)?;
    out.execute(LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;
    result
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
