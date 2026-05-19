//! Interactive L3 storybook viewer.
//!
//! Reads the blessed `.snap` files under `crates/tui/tests/storybook/snapshots/`
//! and renders each story's frame with its styles sidecar applied. No story
//! re-execution — the snapshot files contain everything the user would see.
//!
//! All of the format knowledge lives in `smelt_term::SnapshotFrame`:
//! `parse` reconstructs the captured frame losslessly (the `dim:` header
//! on the styles sidecar carries cell dimensions, so wide-char rows
//! whose trailing continuation slot was `trim_end`-stripped still come
//! back intact), and `blit_into` replays the captured cells through the
//! same `Grid` primitives the live app uses. Crossterm is used only for
//! the terminal envelope (alt screen, raw mode, input polling).
//!
//! Keys: `j`/`k` or `↓`/`↑` navigate, `g`/`G` jump to top/bottom,
//! `q` or `Esc` quits.
//!
//! Run from the workspace root:
//!     cargo run -p tui --example stories

use std::collections::HashMap;
use std::io::stdout;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, ExecutableCommand};

use smelt_term::{Color, Compositor, Grid, SnapshotFrame, Style, Theme};
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Debug)]
struct Story {
    group: String,
    name: String,
    full_id: String,
    snap_path: PathBuf,
    styles_path: PathBuf,
}

fn snapshot_dir() -> PathBuf {
    let cwd = std::env::current_dir().expect("cwd");
    let candidate = cwd.join("tests").join("storybook").join("snapshots");
    if candidate.is_dir() {
        return candidate;
    }
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
        if !file_name.ends_with(".snap") || file_name.ends_with(".styles.snap") {
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

/// Strip the insta YAML header from a `.snap` body — `insta`'s
/// concern, not the library's.
fn read_snap_body(path: &Path) -> std::io::Result<String> {
    let raw = std::fs::read_to_string(path)?;
    let mut iter = raw.lines();
    if iter.next() != Some("---") {
        return Ok(raw);
    }
    for line in iter.by_ref() {
        if line == "---" {
            break;
        }
    }
    Ok(iter.collect::<Vec<_>>().join("\n"))
}

/// Load and parse a story's `.snap` + `.styles.snap` files into a
/// faithful `SnapshotFrame`.
fn load_frame(story: &Story) -> SnapshotFrame {
    let text = read_snap_body(&story.snap_path).unwrap_or_default();
    let styles = if story.styles_path.exists() {
        read_snap_body(&story.styles_path).unwrap_or_default()
    } else {
        String::new()
    };
    SnapshotFrame::parse(&text, &styles)
}

// ── List-pane state ───────────────────────────────────────────────

fn group_counts(stories: &[Story]) -> Vec<(String, usize)> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for s in stories {
        *counts.entry(s.group.clone()).or_insert(0) += 1;
    }
    let mut pairs: Vec<_> = counts.into_iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs
}

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

// ── Paint helpers ─────────────────────────────────────────────────

const STYLE_BOLD: Style = Style {
    fg: None,
    bg: None,
    bold: true,
    dim: false,
    italic: false,
    underline: false,
    crossedout: false,
};

const STYLE_DIM: Style = Style {
    fg: None,
    bg: None,
    bold: false,
    dim: true,
    italic: false,
    underline: false,
    crossedout: false,
};

/// Truncate `text` to at most `max_w` visual columns.
fn fit_width(text: &str, max_w: u16) -> (String, u16) {
    let mut out = String::new();
    let mut emitted: u16 = 0;
    for ch in text.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0).max(1) as u16;
        if emitted + cw > max_w {
            break;
        }
        out.push(ch);
        emitted += cw;
    }
    (out, emitted)
}

/// Paint `text` left-aligned at `(x, y)`, truncating to `max_w` and
/// padding with spaces so a styled background extends to the edge.
fn write_line(grid: &mut Grid, x: u16, y: u16, max_w: u16, text: &str, style: Style) {
    let (slice, emitted) = fit_width(text, max_w);
    grid.put_str(x, y, &slice, style);
    if emitted < max_w {
        let pad = " ".repeat((max_w - emitted) as usize);
        grid.put_str(x + emitted, y, &pad, style);
    }
}

fn paint_frame(grid: &mut Grid, app: &App, term_w: u16, term_h: u16) {
    let list_w: u16 = 32;
    let gap: u16 = 1;
    let preview_left = list_w + gap;
    let preview_w = term_w.saturating_sub(preview_left);
    let header_h: u16 = 2;
    let footer_h: u16 = 2;
    let content_top: u16 = header_h;
    let content_h = term_h.saturating_sub(header_h + footer_h);

    // Header.
    let header = format!(
        "smelt storybook — {} stories across {} groups",
        app.stories.len(),
        group_counts(&app.stories).len()
    );
    write_line(grid, 0, 0, term_w, &header, STYLE_BOLD);

    // List pane.
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
        let g_changed = last_group != Some(story.group.as_str());
        last_group = Some(story.group.as_str());
        let selected = i == app.selected;
        let label = if g_changed {
            format!("▸ {} :: {}", story.group, story.name)
        } else {
            format!("    {}", story.name)
        };
        let style = if selected {
            Style {
                fg: Some(Color::Black),
                bg: Some(Color::White),
                ..Style::default()
            }
        } else if g_changed {
            STYLE_DIM
        } else {
            Style::default()
        };
        write_line(grid, 0, row, list_w, &label, style);
    }

    // Vertical separator.
    for r in content_top..(content_top + content_h) {
        grid.put_str(list_w, r, "│", STYLE_DIM);
    }

    // Preview pane.
    if let Some(story) = app.stories.get(app.selected) {
        let frame = load_frame(story);

        write_line(
            grid,
            preview_left,
            content_top,
            preview_w,
            &story.full_id,
            STYLE_BOLD,
        );
        let info = format!("frame: {} × {}", frame.width, frame.height);
        write_line(
            grid,
            preview_left,
            content_top + 1,
            preview_w,
            &info,
            STYLE_DIM,
        );

        // Frame body, framed by a thin rule above + below.
        let body_top = content_top + 3;
        let frame_max_w = preview_w.saturating_sub(2);
        let render_w = frame.width.min(frame_max_w);
        let inner_x = preview_left + 1;
        let max_inner_h = (content_h as usize).saturating_sub(5) as u16;
        let inner_h = frame.height.min(max_inner_h);

        let top_rule = format!("┌{}┐", "─".repeat(render_w as usize));
        grid.put_str(
            preview_left,
            body_top.saturating_sub(1),
            &top_rule,
            STYLE_DIM,
        );

        for r in 0..inner_h {
            grid.put_str(preview_left, body_top + r, "│", STYLE_DIM);
            grid.put_str(inner_x + render_w, body_top + r, "│", STYLE_DIM);
        }

        frame.blit_into(grid, inner_x, body_top);

        let bottom_row = body_top + inner_h;
        if bottom_row < content_top + content_h {
            let rule = format!("└{}┘", "─".repeat(render_w as usize));
            grid.put_str(preview_left, bottom_row, &rule, STYLE_DIM);
        }
    }

    // Footer.
    let footer = "j/k or ↓/↑ navigate · g/G top/bottom · q/Esc quit";
    write_line(grid, 0, term_h.saturating_sub(1), term_w, footer, STYLE_DIM);
}

// ── Main loop ─────────────────────────────────────────────────────

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
    let theme = Theme::default();
    let mut compositor = Compositor::new(size.0, size.1);

    let result = (|| -> std::io::Result<()> {
        let mut dirty = true;
        loop {
            let list_height = (size.1 as usize).saturating_sub(4);
            if dirty {
                compositor.render_with(&theme, &mut out, |grid, _theme| {
                    paint_frame(grid, &app, size.0, size.1);
                })?;
                dirty = false;
            }
            if event::poll(Duration::from_secs(1))? {
                match event::read()? {
                    Event::Key(KeyEvent {
                        code, modifiers, ..
                    }) => match code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => break,
                        KeyCode::Char('j') | KeyCode::Down => {
                            app.move_selection(1, list_height);
                            dirty = true;
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            app.move_selection(-1, list_height);
                            dirty = true;
                        }
                        KeyCode::PageDown => {
                            app.move_selection((list_height as i32) - 2, list_height);
                            dirty = true;
                        }
                        KeyCode::PageUp => {
                            app.move_selection(-((list_height as i32) - 2), list_height);
                            dirty = true;
                        }
                        KeyCode::Char('g') | KeyCode::Home => {
                            app.jump(true, list_height);
                            dirty = true;
                        }
                        KeyCode::Char('G') | KeyCode::End => {
                            app.jump(false, list_height);
                            dirty = true;
                        }
                        _ => {}
                    },
                    Event::Resize(w, h) => {
                        size = (w, h);
                        compositor.resize(w, h);
                        dirty = true;
                    }
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
