//! Shared storybook harness. `story!` emits one `#[test]` per story; body receives `&mut StoryCtx`.

use insta::{assert_snapshot, with_settings};
use smelt_core::buffer::BufCreateOpts;
use smelt_core::clipboard::Clipboard;
use smelt_core::style::Color;
use smelt_core::style::Style;
use tui::smelt_term::layout::Rect;
use tui::smelt_term::{
    BufId, Buffer, Event, EventCtx, LayoutTree, SnapshotFrame, SplitConfig, Theme, Ui, VimMode,
    WinId, WindowViewport,
};

pub struct StoryCtx {
    pub ui: Ui,
    pub vim_mode: VimMode,
    pub clipboard: Clipboard,
    name: String,
    snapshot_index: u32,
}

impl StoryCtx {
    pub fn new(name: &str) -> Self {
        let mut ui = Ui::new();
        ui.set_terminal_size(80, 24);
        // Register a `Visual` style so `Window::auto_selection_ranges` paints the
        // visual selection with a recognisable bg in snapshots.
        ui.theme_mut().set(
            "Visual",
            Style {
                bg: Some(Color::DarkGrey),
                ..Style::default()
            },
        );
        Self {
            ui,
            vim_mode: VimMode::Normal,
            clipboard: Clipboard::null(),
            name: name.to_string(),
            snapshot_index: 0,
        }
    }

    pub fn set_viewport(&mut self, w: u16, h: u16) {
        self.ui.set_terminal_size(w, h);
    }

    pub fn buf(&mut self) -> BufId {
        self.ui.buf_create(BufCreateOpts::default())
    }

    pub fn buf_with_lines<I, S>(&mut self, lines: I) -> BufId
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let id = self.buf();
        let lines: Vec<String> = lines.into_iter().map(Into::into).collect();
        if let Some(b) = self.ui.buf_mut(id) {
            b.set_all_lines(lines);
        }
        id
    }

    #[allow(dead_code)]
    pub fn buf_mut(&mut self, id: BufId) -> &mut Buffer {
        self.ui.buf_mut(id).expect("buffer exists")
    }

    pub fn open_split(&mut self, buf: BufId, config: SplitConfig) -> WinId {
        self.ui.win_open_split(buf, config).expect("buf exists")
    }

    pub fn set_layout(&mut self, tree: LayoutTree) {
        self.ui.set_layout(tree);
    }

    pub fn theme_mut(&mut self) -> &mut Theme {
        self.ui.theme_mut()
    }

    pub fn frame(&mut self) -> SnapshotFrame {
        self.ui.snapshot()
    }

    pub fn press_vim(&mut self, ev: Event) {
        let Some(focus) = self.ui.focus() else {
            return;
        };
        let Some(buf_id) = self.ui.win(focus).map(|w| w.buf) else {
            return;
        };
        let rows: Vec<String> = self
            .ui
            .buf(buf_id)
            .map(|b| {
                (0..b.line_count())
                    .filter_map(|i| b.get_line(i).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let (term_w, term_h) = self.ui.terminal_size();
        let viewport = WindowViewport::new(
            Rect::new(0, 0, term_w, term_h),
            term_w,
            rows.len() as u16,
            0,
            None,
        );
        if let Some(win) = self.ui.win_mut(focus) {
            win.set_vim_enabled(true);
            // Direct field write so we don't clobber any pending vim sub-sequence
            // (e.g. mid-`dd`, `df<x>`). `set_vim_mode` resets pending state, which would
            // turn every `press_vim` call into a sequence break.
            win.vim_mode = self.vim_mode;
            let ctx = EventCtx {
                rows: &rows,
                soft_breaks: &[],
                hard_breaks: &[],
                viewport,
                click_count: 0,
                clipboard: &mut self.clipboard,
            };
            win.handle(ev, ctx);
            self.vim_mode = win.vim_mode;
        }
        let new_text = self
            .ui
            .win(focus)
            .map(|w| w.text.clone())
            .unwrap_or_default();
        let new_lines: Vec<String> = new_text.split('\n').map(String::from).collect();
        if let Some(b) = self.ui.buf_mut(buf_id) {
            b.set_all_lines(new_lines);
        }
        self.repaint_visual_selection(focus);
    }

    fn repaint_visual_selection(&mut self, win: WinId) {
        let mode = self.vim_mode;
        if !matches!(mode, VimMode::Visual | VimMode::VisualLine) {
            return;
        }
        let buf_id = match self.ui.win(win) {
            Some(w) => w.buf,
            None => return,
        };
        let rows: Vec<String> = self
            .ui
            .buf(buf_id)
            .map(|b| {
                (0..b.line_count())
                    .filter_map(|i| b.get_line(i).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let _ = mode;
        let range = self.ui.win(win).and_then(|w| w.selection_range(&rows));
        let Some((start, end)) = range else {
            return;
        };
        let style = Style {
            bg: Some(Color::DarkGrey),
            ..Style::default()
        };
        let mut byte = 0usize;
        if let Some(b) = self.ui.buf_mut(buf_id) {
            b.clear_highlights(0, rows.len());
            for (row, line) in rows.iter().enumerate() {
                let row_start = byte;
                let row_end = row_start + line.len();
                let lo = start.max(row_start).min(row_end);
                let hi = end.max(row_start).min(row_end);
                if lo < hi {
                    let cs = (lo - row_start) as u16;
                    let ce = (hi - row_start) as u16;
                    b.add_highlight(row, cs, ce, style);
                }
                byte = row_end + 1;
            }
        }
    }

    pub fn assert_snapshot(&mut self) {
        let frame = self.frame();
        let suffix = if self.snapshot_index == 0 {
            String::new()
        } else {
            format!(".step-{}", self.snapshot_index)
        };
        self.snapshot_index += 1;
        let text_name = format!("{}{}", self.name, suffix);
        let style_name = format!("{}{}.styles", self.name, suffix);
        with_settings!({
            prepend_module_to_snapshot => false,
            snapshot_path => "snapshots",
        }, {
            assert_snapshot!(text_name, frame.text());
            assert_snapshot!(style_name, frame.styles_text());
        });
    }
}

#[macro_export]
macro_rules! story {
    ($name:ident, |$ctx:ident| $body:block) => {
        #[test]
        fn $name() {
            let snapshot_id = format!(
                "{}::{}",
                module_path!().rsplit("::").next().unwrap_or("story"),
                stringify!($name),
            );
            let mut __sb_storyctx = $crate::storybook::StoryCtx::new(&snapshot_id);
            let $ctx: &mut $crate::storybook::StoryCtx = &mut __sb_storyctx;
            $body
        }
    };
}

pub mod stories;
