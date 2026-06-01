//! Shared storybook harness for the pure-term snapshot tests.
//!
//! Each story wires a `Compositor`, paints into its grid via
//! `paint_layout_tree` against a story-specific layout tree, and
//! captures a `SnapshotFrame` for `insta`. Two snapshots land per call:
//! `<name>.snap` for the visible glyph grid and `<name>.styles.snap`
//! for the per-cell style sidecar.

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::Arc;

use insta::{assert_snapshot, with_settings};
use smelt_term::{
    paint_layout_tree, Compositor, Grid, GridSlice, LayoutTree, PaintId, Rect, SnapshotFrame,
    Style, Theme,
};

/// Discard writer - the compositor writes SGR escapes during render
/// but the storybook only cares about the post-render grid.
pub struct Discard;

impl Write for Discard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Per-story context. Holds the compositor, the theme, the viewport
/// size, and a `PaintId → text-lines` map so leaf dispatch can paint
/// each leaf's content without hard-coding it into the harness.
pub struct StoryCtx {
    pub name: String,
    pub width: u16,
    pub height: u16,
    pub compositor: Compositor,
    pub theme: Arc<Theme>,
    pub leaf_text: HashMap<PaintId, Vec<String>>,
}

impl StoryCtx {
    pub fn new(name: &str) -> Self {
        Self::with_size(name, 80, 24)
    }

    pub fn with_size(name: &str, w: u16, h: u16) -> Self {
        Self {
            name: name.to_string(),
            width: w,
            height: h,
            compositor: Compositor::new(w, h),
            theme: Arc::new(Theme::new()),
            leaf_text: HashMap::new(),
        }
    }

    pub fn set_viewport(&mut self, w: u16, h: u16) {
        self.width = w;
        self.height = h;
        self.compositor.resize(w, h);
    }

    /// Register text lines for a leaf id. The default leaf paint
    /// dispatcher writes these lines at row 0, col 0 of the leaf rect.
    pub fn set_leaf<I, S>(&mut self, id: PaintId, lines: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.leaf_text
            .insert(id, lines.into_iter().map(Into::into).collect());
    }

    /// Paint the registered tree against `area`. Each leaf id is
    /// resolved via [`Self::set_leaf`]; missing ids paint nothing.
    pub fn paint_tree(&mut self, tree: &LayoutTree, area: Rect) {
        self.paint_passes(&[(tree, area, false)]);
    }

    /// Render two layered passes in one frame: first `backdrop` covers
    /// the whole viewport, then `chrome_tree` paints over it at `inner`.
    /// Replicates the original chrome storybook shape (a dotted backdrop
    /// plus an overlay-positioned bordered container) without depending
    /// on the editor's overlay machinery. The chrome pass clears each
    /// leaf to spaces first so the backdrop doesn't bleed through cells
    /// the leaf-text doesn't reach.
    pub fn paint_backdrop_then_chrome(
        &mut self,
        backdrop: &LayoutTree,
        chrome_tree: &LayoutTree,
        inner: Rect,
    ) {
        let area = Rect::new(0, 0, self.width, self.height);
        self.paint_passes(&[(backdrop, area, false), (chrome_tree, inner, true)]);
    }

    /// Render `passes` in order into the same frame. Each pass is
    /// `(tree, area, clear_leaf)`: `clear_leaf=true` fills each leaf
    /// rect with spaces before painting its registered text, so a
    /// later overlay pass overwrites whatever the earlier passes left.
    fn paint_passes(&mut self, passes: &[(&LayoutTree, Rect, bool)]) {
        let leaf_text = self.leaf_text.clone();
        let theme = Arc::clone(&self.theme);
        let term_size = (self.width, self.height);
        let mut writer = Discard;
        self.compositor
            .render_with(&self.theme, &mut writer, |grid, _theme| {
                for &(tree, area, clear_leaf) in passes {
                    let mut dispatch = |id: PaintId,
                                        leaf: Rect,
                                        grid: &mut Grid,
                                        _t: &Arc<Theme>,
                                        _ts: (u16, u16)| {
                        let mut slice: GridSlice = grid.slice_mut(leaf);
                        if clear_leaf {
                            let leaf_area = slice.area();
                            let local = Rect::new(0, 0, leaf_area.width, leaf_area.height);
                            slice.fill(local, ' ', Style::default());
                        }
                        if let Some(lines) = leaf_text.get(&id) {
                            for (y, line) in lines.iter().enumerate() {
                                if y as u16 >= slice.height() {
                                    break;
                                }
                                slice.put_str(0, y as u16, line, Style::default());
                            }
                        }
                    };
                    paint_layout_tree(grid, &theme, tree, area, term_size, &mut dispatch);
                }
            })
            .expect("render");
    }

    /// Capture the frame just rendered.
    pub fn frame(&self) -> SnapshotFrame {
        SnapshotFrame::from_grid(self.compositor.previous())
    }

    pub fn assert_snapshot(&self) {
        let frame = self.frame();
        let text_name = self.name.clone();
        let style_name = format!("{}.styles", self.name);
        with_settings!({
            prepend_module_to_snapshot => false,
            snapshot_path => "snapshots",
        }, {
            assert_snapshot!(text_name, frame.text());
            assert_snapshot!(style_name, frame.styles_text());
        });
    }
}

pub mod chrome;
pub mod layout;

/// Define a story as a `#[test]` function. Body receives `&mut
/// StoryCtx`. The macro auto-derives the snapshot id from the
/// containing module's last segment + the story name.
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
