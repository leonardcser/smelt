//! Shared L3 storybook harness. Imported by the `storybook_main.rs`
//! test crate via `#[macro_use] mod storybook;`. The macro `story!`
//! emits one `#[test]` per story and takes a closure body that
//! mutates a fresh `StoryCtx`.
//!
//! L3-prim only for the moment: `StoryCtx` carries a `Ui` plus
//! `Theme` plus viewport. L3-comp (Lua + MockEngine) lands when the
//! first component story needs it.

use insta::{assert_snapshot, with_settings};
use smelt_core::buffer::BufCreateOpts;
use tui::ui::{BufId, Buffer, LayoutTree, SnapshotFrame, SplitConfig, Theme, Ui, WinId};

/// Test-time context passed to every story. Owns the `Ui` and
/// viewport; stories drive it through helpers and call
/// [`StoryCtx::assert_snapshot`] when ready.
pub struct StoryCtx {
    pub ui: Ui,
    name: String,
    snapshot_index: u32,
}

impl StoryCtx {
    /// Fresh context with a default 80×24 viewport, empty splits, no
    /// overlays. `name` is the snapshot id (group + story).
    pub fn new(name: &str) -> Self {
        let mut ui = Ui::new();
        ui.set_terminal_size(80, 24);
        Self {
            ui,
            name: name.to_string(),
            snapshot_index: 0,
        }
    }

    /// Resize the terminal viewport. Call before painting.
    pub fn set_viewport(&mut self, w: u16, h: u16) {
        self.ui.set_terminal_size(w, h);
    }

    /// Mint a fresh empty buffer.
    pub fn buf(&mut self) -> BufId {
        self.ui.buf_create(BufCreateOpts::default())
    }

    /// Mint a buffer pre-populated with the given lines.
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

    /// Mutable access to a buffer for in-line editing. Reserved for
    /// stories that mutate after creation; not consumed by any L3-prim
    /// story today.
    #[allow(dead_code)]
    pub fn buf_mut(&mut self, id: BufId) -> &mut Buffer {
        self.ui.buf_mut(id).expect("buffer exists")
    }

    /// Open a window over a buffer, configured as a splits leaf.
    pub fn open_split(&mut self, buf: BufId, config: SplitConfig) -> WinId {
        self.ui.win_open_split(buf, config).expect("buf exists")
    }

    /// Replace the splits layout. Pair with [`StoryCtx::open_split`]
    /// to wire leaves into the tree.
    pub fn set_layout(&mut self, tree: LayoutTree) {
        self.ui.set_layout(tree);
    }

    /// Mutable theme access. Stories use this to swap the accent or
    /// override individual highlight groups.
    pub fn theme_mut(&mut self) -> &mut Theme {
        self.ui.theme_mut()
    }

    /// Render and capture one frame.
    pub fn frame(&mut self) -> SnapshotFrame {
        self.ui.snapshot()
    }

    /// Render and snapshot the resulting frame. Two `insta` snapshots
    /// land per call: `<story>.snap` for the text (rows joined by
    /// `\n`) and `<story>.styles.snap` for the per-cell style
    /// sidecar. Style-only changes touch only the styles file; wrap
    /// regressions touch only the text file.
    ///
    /// Multi-step stories may call this more than once; each
    /// invocation gets a unique suffix (`step-0`, `step-1`, …) so
    /// snapshots don't collide.
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

/// Define a story as a `#[test]` function. Body receives `&mut
/// StoryCtx`. The macro auto-derives the snapshot id from the
/// containing module path + the story name.
///
/// Example:
/// ```ignore
/// story!(vbox_basic, |ctx| {
///     // …
///     ctx.assert_snapshot();
/// });
/// ```
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
