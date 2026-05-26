//! Buffer — lines + namespaced extmarks.
//!
//! Mirrors `nvim_buf_set_extmark`: a `Buffer` holds text lines plus
//! `Extmark`s grouped into namespaces. Highlights, decorations, and
//! virtual text are all extmarks queried per-line at render time.

use crate::attachment::{AttachmentId, ATTACHMENT_MARKER};
use crate::undo::UndoHistory;
use smelt_style::style::{Color, Style};
use smelt_style::theme::{intern_anonymous_style, HlGroup};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// Buffer handle. IDs below `LUA_BUF_ID_BASE` are Rust-minted; IDs at
/// or above are plugin-minted. Collisions surface as a loud notify.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BufId(pub u64);

impl BufId {
    pub fn raw(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for BufId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "buf:{}", self.0)
    }
}

/// Smallest id minted by plugin-side `smelt.buf.new`. Keeps Lua
/// buffers in a disjoint range from Rust's sequential allocator.
pub const LUA_BUF_ID_BASE: u64 = 1 << 32;

/// Parser attached to a `Buffer`. Rebuilds lines, extmarks, and
/// decorations from a source string whenever `(source_tick, width)`
/// changes. `on_attach` fires once at installation for namespace setup.
///
/// Parsers whose source bytes don't map 1:1 to display chars (e.g. the prompt,
/// where attachment markers expand to `[label]`) should write a
/// [`ProjectionMaps`](crate::coords::ProjectionMaps) into the buffer via
/// [`Buffer::set_projection_maps`] inside `parse`. Parsers that don't write
/// maps fall through to identity coord mapping.
pub trait BufferParser: Send + Sync {
    /// Rebuild the buffer's lines / extmarks / decorations from
    /// `source` at the given render `width`.
    fn parse(&self, buf: &mut Buffer, source: &str, width: u16);

    /// Called once when the parser is installed. Default no-op.
    fn on_attach(&self, _buf: &mut Buffer) {}
}

/// Two outputs from a single yank: the kill-ring text (paste-back fidelity,
/// e.g. raw `\u{FFFC}` attachment markers survive `y`/`p`) and the system
/// clipboard text (human-readable, e.g. markers expand to `[label]`).
///
/// Buffers without a [`BufferCopy`] impl produce identical outputs equal to
/// the raw source slice.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CopyOutput {
    pub kill_ring: String,
    pub clipboard: String,
}

impl CopyOutput {
    /// Both outputs equal `s`. The default identity case for buffers without
    /// a [`BufferCopy`] installed.
    pub fn same(s: String) -> Self {
        Self {
            kill_ring: s.clone(),
            clipboard: s,
        }
    }

    /// `true` when both outputs are empty (the only state callers should skip).
    pub fn is_empty(&self) -> bool {
        self.kill_ring.is_empty() && self.clipboard.is_empty()
    }

    /// `(kill_ring, clipboard)`. Useful when both halves need to be moved
    /// into different sinks without intermediate clones.
    pub fn into_parts(self) -> (String, String) {
        (self.kill_ring, self.clipboard)
    }
}

/// Per-buffer transform from a byte range to a [`CopyOutput`].
///
/// `src` is the resolved base string [`Buffer::copy_range`] picked (`source`
/// when non-empty, else `text()`); `range` is in-bounds, on char boundaries,
/// and addresses bytes in `src`. The buffer is passed alongside so impls can
/// read sidecar state (attachment ids, decorations).
pub trait BufferCopy: Send + Sync {
    fn copy(&self, buf: &Buffer, src: &str, range: std::ops::Range<usize>) -> CopyOutput;
}

/// Process-global namespace id. Stable for the process lifetime;
/// the same name always yields the same id across every `Buffer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NsId(pub u32);

fn namespace_registry() -> &'static std::sync::RwLock<NamespaceRegistry> {
    use std::sync::{OnceLock, RwLock};
    static REG: OnceLock<RwLock<NamespaceRegistry>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(NamespaceRegistry::default()))
}

/// Idempotent name → id minter. Same `name` always returns the same id.
pub fn create_namespace(name: &str) -> NsId {
    let reg = namespace_registry();
    if let Some(id) = reg.read().unwrap().name_to_id.get(name).copied() {
        return id;
    }
    let mut w = reg.write().unwrap();
    if let Some(id) = w.name_to_id.get(name).copied() {
        return id;
    }
    let id = NsId(w.name_to_id.len() as u32);
    w.name_to_id.insert(name.to_string(), id);
    id
}

/// Reset the process-global namespace interner. Intended for
/// deterministic-simulation tests and fuzz harnesses that reuse one
/// process across scenarios; in production this map grows monotonically
/// and is never reset.
pub fn reset_namespaces_for_test() {
    namespace_registry().write().unwrap().name_to_id.clear();
}

/// Current number of interned namespace ids. Used by leak invariants to
/// confirm that scenario teardown returns the registry to its baseline.
pub fn namespace_count() -> usize {
    namespace_registry().read().unwrap().name_to_id.len()
}

#[derive(Default)]
struct NamespaceRegistry {
    name_to_id: HashMap<String, NsId>,
}

/// Identifier returned by `Buffer::set_extmark`. Unique within its namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExtmarkId(pub u32);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpanMeta {
    pub selectable: bool,
    pub copy_as: Option<String>,
}

impl Default for SpanMeta {
    fn default() -> Self {
        Self {
            selectable: true,
            copy_as: None,
        }
    }
}

/// Per-row mapping back to a "source line number" — what a gutter provider
/// (`LineNumberGutter`) renders for that row. `None` on the decoration falls back
/// to the row index + 1 so a plain text buffer needs no setup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceLine {
    /// 1-based file line number. Use for plain code buffers and the post-edit side
    /// of a diff where every row corresponds to a real file line.
    Linear { lineno: u32 },
    /// Both old and new line numbers. Either side is `None` when the row exists only
    /// on the other (added: `old=None`; removed: `new=None`).
    Diff { old: Option<u32>, new: Option<u32> },
    /// Header / hunk-separator / blank divider rows that aren't line-numbered.
    Synthetic,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LineDecoration {
    /// Row-level bg fill, painted across the entire slice width by `Window::render`.
    /// Set via `Buffer::set_decoration` for buffers that aren't built through
    /// `LineBuilder` (e.g. the cmdline status bar). Transcript content blocks
    /// pad with inline styled spaces via `LineBuilder::pad_row_to_layout_width`
    /// instead, so the bg, content, and chrome cells share one mechanism.
    pub fill_bg: Option<Color>,
    pub soft_wrapped: bool,
    /// Rows with this bit opt into chrome-delimited region selection. A
    /// double-click can select the selectable run between neighboring
    /// non-selectable spans. Renderers use this for structured rows such as
    /// Markdown table data rows without making the window inspect glyphs.
    pub cell_selectable: bool,
    /// Rows with this bit opt into contiguous block selection. A triple-click
    /// expands through adjacent rows with the same bit. This is generic row
    /// metadata for preformatted structures, not transcript-specific behavior.
    pub block_selectable: bool,
    /// When `true`, `copy_byte_range` treats this row as part of the previous
    /// row's copy group: it skips the newline and, if `source_text` was already
    /// emitted from the group's first row, skips this row entirely. This is
    /// orthogonal to `soft_wrapped` — table rows use `copy_continuation` without
    /// `soft_wrapped` so each display row is a hard selection boundary while
    /// still coalescing into a single `source_text` on copy.
    pub copy_continuation: bool,
    pub source_text: Option<String>,
    /// Logical line mapping for this row. `None` = fall back to `row + 1`.
    pub source_line: Option<SourceLine>,
    /// `true` when the row's content was already laid out at the producer's
    /// chosen width (parser output, markdown tables, diff hunks). The host
    /// window's `WrappedLayout` skips wrapping these rows so the producer's
    /// layout is preserved verbatim.
    pub pre_formatted: bool,
}

pub type SpanStyle = Style;

/// Materialized highlight span for one line, returned by `Buffer::highlights_at`.
/// Carries an interned [`HlGroup`] id; the theme resolves it at paint time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub col_start: u16,
    pub col_end: u16,
    pub hl: HlGroup,
    pub meta: SpanMeta,
    /// When true, background extends past `col_end` to the right edge of the row.
    pub hl_eol: bool,
    /// When true, the renderer applies this span only on the window's cursor row.
    /// Use for selection-aware decoration (e.g. accent fg on the selected option in a list).
    pub on_cursor_row: bool,
}

/// One-line virtual text overlay. Derived from `ExtmarkPayload::VirtText` marks.
#[derive(Clone, Debug)]
pub struct VirtualText {
    pub col: usize,
    pub text: String,
    pub hl_group: Option<String>,
    pub pos: VirtTextPos,
}

/// Per-row visual-mode selection range in content columns (before gutter offset).
/// Empty lines use `col_start = 0, col_end = 1` to paint one virtual cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SelectionRange {
    pub line: usize,
    pub col_start: u16,
    pub col_end: u16,
}

#[derive(Default)]
pub struct BufCreateOpts {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineEdit {
    pub before_tick: u64,
    pub after_tick: u64,
    pub start: usize,
    pub old_end: usize,
    pub old_line_count: usize,
    pub new_end: usize,
}

// ─── Extmark model ─────────────────────────────────────────────────

/// How a Highlight extmark blends with its row's existing background.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HlMode {
    /// Replace the existing bg outright (default).
    #[default]
    Replace,
    /// Combine fg/bg attributes over the existing row paint.
    Combine,
    /// Blend (alpha) — currently treated as Combine.
    Blend,
}

/// Placement of inline virtual text relative to the mark's column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VirtTextPos {
    /// Append at end of line content.
    #[default]
    Eol,
    /// Insert at `start_col`, shifting real content right.
    Inline,
    /// Overlay at `start_col`, replacing real content cells.
    Overlay,
    /// Right-align at `width - virt_text_width`.
    RightAlign,
}

/// Positional anchor with a payload. Addressable by `(NsId, ExtmarkId)`.
#[derive(Clone, Debug)]
pub struct Extmark {
    pub start_row: usize,
    pub start_col: usize,
    pub end_row: usize,
    pub end_col: usize,
    pub payload: ExtmarkPayload,
    /// Paint priority. Higher paints on top; ties break by ns-id then insertion order.
    pub priority: u32,
    /// Start anchor sticks right of an insertion at its column. Default `true`.
    pub right_gravity: bool,
    /// End anchor sticks right of an insertion at its column. Default `false`.
    pub end_right_gravity: bool,
}

/// Payload carried by an extmark.
#[derive(Clone, Debug)]
pub enum ExtmarkPayload {
    Highlight {
        hl: HlGroup,
        meta: SpanMeta,
        /// Extend highlight to end-of-line even if `end_col` is shorter.
        hl_eol: bool,
        /// How this highlight blends with the row's existing paint.
        hl_mode: HlMode,
        /// Replace each visible cell in the range with this string (single grapheme).
        conceal: Option<String>,
        /// When true, paint only on the window's cursor row. Used for selection-aware
        /// decoration (e.g. accent fg on a list's selected label) without per-event
        /// re-rendering.
        on_cursor_row: bool,
    },
    Decoration(LineDecoration),
    VirtText {
        text: String,
        hl_group: Option<String>,
        pos: VirtTextPos,
    },
}

/// Options for `set_extmark`. `end_row`/`end_col` default to the start position.
#[derive(Clone, Debug)]
pub struct ExtmarkOpts {
    pub end_row: Option<usize>,
    pub end_col: Option<usize>,
    pub payload: ExtmarkPayload,
    pub priority: u32,
    pub right_gravity: bool,
    pub end_right_gravity: bool,
    /// When set, replace this mark id instead of allocating a new one.
    pub id: Option<ExtmarkId>,
}

impl ExtmarkOpts {
    /// Interns `style` as a content-hashed [`HlGroup`]. Prefer
    /// [`Self::highlight_group`] for theme-reactive styles.
    pub fn highlight(end_col: usize, style: SpanStyle, meta: SpanMeta) -> Self {
        Self::highlight_group(end_col, intern_anonymous_style(style), meta)
    }

    pub fn highlight_group(end_col: usize, hl: HlGroup, meta: SpanMeta) -> Self {
        Self {
            end_row: None,
            end_col: Some(end_col),
            payload: ExtmarkPayload::Highlight {
                hl,
                meta,
                hl_eol: false,
                hl_mode: HlMode::Replace,
                conceal: None,
                on_cursor_row: false,
            },
            priority: 0,
            right_gravity: true,
            end_right_gravity: false,
            id: None,
        }
    }

    pub fn decoration(dec: LineDecoration) -> Self {
        Self {
            end_row: None,
            end_col: None,
            payload: ExtmarkPayload::Decoration(dec),
            priority: 0,
            right_gravity: true,
            end_right_gravity: false,
            id: None,
        }
    }

    pub fn virt_text(text: String, hl_group: Option<String>) -> Self {
        Self {
            end_row: None,
            end_col: None,
            payload: ExtmarkPayload::VirtText {
                text,
                hl_group,
                pos: VirtTextPos::Eol,
            },
            priority: 0,
            right_gravity: true,
            end_right_gravity: false,
            id: None,
        }
    }

    /// Set paint priority. Higher paints on top.
    pub fn with_priority(mut self, priority: u32) -> Self {
        self.priority = priority;
        self
    }

    /// Re-target an existing extmark id instead of minting a new one.
    pub fn with_id(mut self, id: ExtmarkId) -> Self {
        self.id = Some(id);
        self
    }

    /// Limit this highlight to the cursor row of whatever window is rendering the buffer.
    /// No-op for non-Highlight payloads.
    pub fn with_on_cursor_row(mut self, on_cursor_row: bool) -> Self {
        if let ExtmarkPayload::Highlight {
            on_cursor_row: ref mut flag,
            ..
        } = &mut self.payload
        {
            *flag = on_cursor_row;
        }
        self
    }

    /// Extend the highlight to end-of-line. No-op for non-Highlight payloads.
    pub fn with_hl_eol(mut self, hl_eol: bool) -> Self {
        if let ExtmarkPayload::Highlight {
            hl_eol: ref mut e, ..
        } = &mut self.payload
        {
            *e = hl_eol;
        }
        self
    }

    /// Set virt_text position. No-op for non-VirtText payloads.
    pub fn with_virt_pos(mut self, pos: VirtTextPos) -> Self {
        if let ExtmarkPayload::VirtText { pos: ref mut p, .. } = &mut self.payload {
            *p = pos;
        }
        self
    }
}

#[derive(Default, Clone)]
struct NamespaceState {
    extmarks: BTreeMap<ExtmarkId, Extmark>,
    /// Secondary index: row → ids on that row. Lets `highlights_at`,
    /// `virtual_text_at`, and `decoration_at` skip the full extmark scan
    /// (was O(N), now O(marks_on_row)). Kept in sync by the helpers below.
    by_row: BTreeMap<usize, Vec<ExtmarkId>>,
    next_id: u32,
}

impl NamespaceState {
    fn insert_mark(&mut self, id: ExtmarkId, mark: Extmark) {
        let row = mark.start_row;
        if let Some(prev) = self.extmarks.insert(id, mark) {
            self.row_remove(prev.start_row, id);
        }
        self.by_row.entry(row).or_default().push(id);
    }

    fn remove_mark(&mut self, id: ExtmarkId) -> Option<Extmark> {
        let mark = self.extmarks.remove(&id)?;
        self.row_remove(mark.start_row, id);
        Some(mark)
    }

    fn row_remove(&mut self, row: usize, id: ExtmarkId) {
        if let Some(ids) = self.by_row.get_mut(&row) {
            if let Some(pos) = ids.iter().position(|x| *x == id) {
                ids.swap_remove(pos);
            }
            if ids.is_empty() {
                self.by_row.remove(&row);
            }
        }
    }

    /// Marks on `row` in `(id, mark)` form. Order is row-insertion order;
    /// callers needing `(priority, ns, id)` sorting apply it themselves.
    fn marks_at(&self, row: usize) -> impl Iterator<Item = (ExtmarkId, &Extmark)> + '_ {
        self.by_row.get(&row).into_iter().flat_map(move |ids| {
            ids.iter()
                .filter_map(|id| self.extmarks.get(id).map(|m| (*id, m)))
        })
    }
}

#[derive(Default, Clone)]
struct ExtmarkStore {
    /// `BTreeMap` so iteration is sorted by `NsId`, matching the priority/ns/id
    /// tiebreak that highlight and virt-text queries assume.
    namespaces: BTreeMap<NsId, NamespaceState>,
}

impl ExtmarkStore {
    fn create_namespace(&mut self, name: &str) -> NsId {
        let id = create_namespace(name);
        self.namespaces.entry(id).or_default();
        id
    }

    fn ns_mut(&mut self, ns: NsId) -> &mut NamespaceState {
        self.namespaces.entry(ns).or_default()
    }

    fn ns(&self, ns: NsId) -> Option<&NamespaceState> {
        self.namespaces.get(&ns)
    }

    fn set_extmark(&mut self, ns: NsId, mark: Extmark) -> ExtmarkId {
        let state = self.ns_mut(ns);
        let id = ExtmarkId(state.next_id);
        state.next_id += 1;
        state.insert_mark(id, mark);
        id
    }

    fn replace_extmark(&mut self, ns: NsId, id: ExtmarkId, mark: Extmark) {
        let state = self.ns_mut(ns);
        state.insert_mark(id, mark);
        if id.0 >= state.next_id {
            state.next_id = id.0 + 1;
        }
    }

    fn del_extmark(&mut self, ns: NsId, id: ExtmarkId) -> Option<Extmark> {
        self.namespaces.get_mut(&ns)?.remove_mark(id)
    }

    fn clear_namespace(&mut self, ns: NsId, line_start: usize, line_end: usize) {
        let Some(state) = self.namespaces.get_mut(&ns) else {
            return;
        };
        let to_remove: Vec<ExtmarkId> = state
            .extmarks
            .iter()
            .filter(|(_, m)| overlaps_lines(m, line_start, line_end))
            .map(|(id, _)| *id)
            .collect();
        for id in to_remove {
            state.remove_mark(id);
        }
    }
}

fn overlaps_lines(m: &Extmark, line_start: usize, line_end: usize) -> bool {
    let m_end = m.end_row.max(m.start_row);
    m.start_row < line_end && m_end >= line_start
}

#[derive(Clone)]
pub struct Buffer {
    pub(crate) id: BufId,
    /// Arc-wrapped so clones and sync-to-view are cheap refcount bumps;
    /// `Arc::make_mut` deep-copies only when the Arc is shared.
    lines: Arc<Vec<String>>,
    extmarks: ExtmarkStore,
    /// Bumped on every lines mutation.
    changedtick: u64,
    last_line_edit: Option<LineEdit>,
    /// Interned at construction so convenience methods skip a hashmap lookup.
    ns_highlights: NsId,
    ns_decorations: NsId,
    ns_virt_text: NsId,
    parser: Option<Arc<dyn BufferParser>>,
    copier: Option<Arc<dyn BufferCopy>>,
    source: String,
    source_tick: u64,
    last_render: Option<(u64, u16)>,
    pub history: UndoHistory,
    pub attachment_ids: Vec<AttachmentId>,
    pub readonly: bool,
    selection: Vec<SelectionRange>,
    /// Source↔display coord maps. Written by parsers in `parse()` when source
    /// bytes don't map 1:1 to display chars. `None` falls back to identity.
    projection_maps: Option<crate::coords::ProjectionMaps>,
}

impl Buffer {
    pub const NS_HIGHLIGHTS: &'static str = "buffer.highlights";
    pub const NS_DECORATIONS: &'static str = "buffer.decorations";
    /// Convenience namespace for `set_virtual_text`. Production virt-text
    /// uses per-feature namespaces via `set_extmark` + `ExtmarkOpts::virt_text`.
    pub const NS_VIRT_TEXT: &'static str = "buffer.virt_text";

    pub fn new(id: BufId, _opts: BufCreateOpts) -> Self {
        let mut extmarks = ExtmarkStore::default();
        let ns_highlights = extmarks.create_namespace(Self::NS_HIGHLIGHTS);
        let ns_decorations = extmarks.create_namespace(Self::NS_DECORATIONS);
        let ns_virt_text = extmarks.create_namespace(Self::NS_VIRT_TEXT);
        Self {
            id,
            lines: Arc::new(vec![String::new()]),
            extmarks,
            changedtick: 0,
            last_line_edit: None,
            ns_highlights,
            ns_decorations,
            ns_virt_text,
            parser: None,
            copier: None,
            source: String::new(),
            source_tick: 0,
            last_render: None,
            history: UndoHistory::default(),
            attachment_ids: Vec::new(),
            readonly: false,
            selection: Vec::new(),
            projection_maps: None,
        }
    }

    /// Override the per-row visual-mode selection. Empty `ranges` clears
    /// the override; `Window::render` then derives selection from its own state.
    pub fn set_selection(&mut self, ranges: Vec<SelectionRange>) {
        if self.selection == ranges {
            return;
        }
        self.selection = ranges;
    }

    /// Returns the selection override set by [`Self::set_selection`]. Empty when inactive.
    pub fn selection(&self) -> &[SelectionRange] {
        &self.selection
    }

    pub fn clear_selection(&mut self) {
        self.selection.clear();
    }

    /// Attach a parser, firing `on_attach` once and invalidating the render cache.
    pub fn has_parser(&self) -> bool {
        self.parser.is_some()
    }

    pub fn set_parser(&mut self, parser: Arc<dyn BufferParser>) {
        parser.on_attach(self);
        self.parser = Some(parser);
        self.last_render = None;
    }

    /// Builder form of `set_parser`.
    pub fn attach(mut self, parser: Arc<dyn BufferParser>) -> Self {
        self.set_parser(parser);
        self
    }

    /// Install a [`BufferCopy`] sidecar. Without one, [`Self::copy_range`]
    /// returns the raw source slice for both outputs.
    pub fn set_copier(&mut self, copier: Arc<dyn BufferCopy>) {
        self.copier = Some(copier);
    }

    pub fn has_copier(&self) -> bool {
        self.copier.is_some()
    }

    /// Push the most recent kill-ring source range to the system clipboard,
    /// transformed by this buffer's [`BufferCopy`]. Idempotent; no-op when
    /// the kill ring lacks a source range or yields empty output. Records
    /// the write on the kill ring so external-clipboard-update detection
    /// stays correct.
    pub fn sync_clipboard_from_kill_ring(&self, clipboard: &mut crate::clipboard::Clipboard) {
        let Some((start, end)) = clipboard.kill_ring.source_range() else {
            return;
        };
        let out = self.copy_range(start..end);
        let kill_text = clipboard.kill_ring.current();

        // If the buffer was mutated after the yank (e.g. vim d/x), the text at
        // source_range no longer matches what was captured. Fall back to the
        // kill-ring text so we copy the actual deleted/yanked text, not whatever
        // shifted into its place.
        let clipboard_text = if out.kill_ring.replace(ATTACHMENT_MARKER, "") == kill_text {
            out.clipboard
        } else {
            kill_text.to_string()
        };

        if clipboard_text.is_empty() {
            return;
        }
        if clipboard.write(&clipboard_text).is_ok() {
            clipboard.kill_ring.record_clipboard_write(clipboard_text);
        }
    }

    /// Snapshot a byte range as a [`CopyOutput`]. The range is in the buffer's
    /// editable-byte space — `source` when the parser writes it, otherwise
    /// `lines.join("\n")`. Endpoints are snapped and clamped; buffers without
    /// a copier fall through to identity.
    pub fn copy_range(&self, range: std::ops::Range<usize>) -> CopyOutput {
        let owned;
        let src: &str = if self.source.is_empty() {
            owned = self.text();
            &owned
        } else {
            &self.source
        };
        let len = src.len();
        let start = crate::text::snap(src, range.start.min(len));
        let end = crate::text::snap(src, range.end.min(len));
        if start >= end {
            return CopyOutput::default();
        }
        let clamped = start..end;
        match self.copier.clone() {
            Some(c) => c.copy(self, src, clamped),
            None => CopyOutput::same(src[clamped].to_string()),
        }
    }

    /// Read the editable source text.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Mutable access to source + attachment ids as a single
    /// invariant-preserving wrapper. Use this instead of raw `&mut String`
    /// for any mutation that might add/remove attachment markers — the
    /// wrapper drains/inserts ids automatically. Bumps `source_tick`.
    pub fn text_mut(&mut self) -> crate::attached::AttachedTextMut<'_> {
        self.source_tick = self.source_tick.wrapping_add(1);
        crate::attached::AttachedTextMut::new(&mut self.source, &mut self.attachment_ids)
    }

    /// Split-mutable refs for a single edit step: text wrapper + undo
    /// history. Bumps `source_tick` (callers must follow with
    /// `sync_after_edit` to refresh `lines`). For buffers without a parser,
    /// lazily rebuilds `source` from `lines.join("\n")` on first call.
    pub fn edit_refs(&mut self) -> (crate::attached::AttachedTextMut<'_>, &mut UndoHistory) {
        if self.parser.is_none() && self.source.is_empty() && !self.lines_is_blank() {
            self.source = self.lines.join("\n");
        }
        self.source_tick = self.source_tick.wrapping_add(1);
        (
            crate::attached::AttachedTextMut::new(&mut self.source, &mut self.attachment_ids),
            &mut self.history,
        )
    }

    fn lines_is_blank(&self) -> bool {
        self.lines.iter().all(|s| s.is_empty())
    }

    /// Update the source driving the parser. No-op if source is unchanged.
    pub fn set_source(&mut self, source: String) {
        if source == self.source {
            return;
        }
        self.source = source;
        self.source_tick = self.source_tick.wrapping_add(1);
    }

    /// Re-render at `width`. With a parser, re-runs `parse` if `(source_tick,
    /// width)` is stale. Without a parser, no-op — caller-written lines are
    /// final and wrap (if any) is owned by the host window.
    ///
    /// Returns `true` when a re-render actually happened.
    pub fn ensure_rendered_at(&mut self, width: u16) -> bool {
        let Some(parser) = self.parser.clone() else {
            return false;
        };
        let fresh = match self.last_render {
            Some((tick, w)) => tick == self.source_tick && w == width,
            None => false,
        };
        if fresh {
            return false;
        }
        let source = std::mem::take(&mut self.source);
        // Reset to a single seed line; `set_all_lines` clears well-known
        // namespaces across all rows so stale highlights don't leak.
        // Maps cleared too — parser repopulates if it needs custom mapping.
        self.set_all_lines(vec![String::new()]);
        self.projection_maps = None;
        parser.parse(self, &source, width);
        self.source = source;
        self.last_render = Some((self.source_tick, width));
        true
    }

    pub fn id(&self) -> BufId {
        self.id
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn get_lines(&self, start: usize, end: usize) -> &[String] {
        let end = end.min(self.lines.len());
        let start = start.min(end);
        &self.lines[start..end]
    }

    pub fn get_line(&self, idx: usize) -> Option<&str> {
        self.lines.get(idx).map(|s| s.as_str())
    }

    pub fn set_lines(&mut self, start: usize, end: usize, replacement: Vec<String>) {
        let end = end.min(self.lines.len());
        let start = start.min(end);
        let old_line_count = self.lines.len();
        let replacement_len = replacement.len();
        let before_tick = self.changedtick;
        let lines = Arc::make_mut(&mut self.lines);
        lines.splice(start..end, replacement);
        let inserted_len = if lines.is_empty() {
            lines.push(String::new());
            1
        } else {
            replacement_len
        };
        // Clear extmarks in the replaced range (wholesale line replacement
        // drops all marks in the slice, mirroring nvim's behavior).
        for ns in [self.ns_highlights, self.ns_decorations, self.ns_virt_text] {
            self.extmarks.clear_namespace(ns, start, end);
        }
        self.selection.retain(|r| r.line < start || r.line >= end);
        self.changedtick += 1;
        self.last_line_edit = Some(LineEdit {
            before_tick,
            after_tick: self.changedtick,
            start,
            old_end: end,
            old_line_count,
            new_end: start + inserted_len,
        });
    }

    pub fn set_all_lines(&mut self, lines: Vec<String>) {
        let new_lines = if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        };
        self.lines = Arc::new(new_lines);
        // Drop well-known namespaces; custom namespaces persist.
        for ns in [self.ns_highlights, self.ns_decorations, self.ns_virt_text] {
            self.extmarks.clear_namespace(ns, 0, usize::MAX);
        }
        self.selection.clear();
        self.changedtick += 1;
        self.last_line_edit = None;
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Extract text in byte range `[start, end)`, applying `SpanMeta` filters:
    /// cells covered by `selectable: false` spans are dropped; `copy_as` spans
    /// emit their substitution string once per span. Rows are joined with `\n`.
    pub fn extract_text(&self, start: usize, end: usize) -> String {
        use unicode_width::UnicodeWidthChar;
        if start >= end {
            return String::new();
        }
        let mut out = String::new();
        let mut line_start = 0usize;
        let total_lines = self.lines.len();
        for (row, line) in self.lines.iter().enumerate() {
            let line_end_byte = line_start + line.len();
            let row_in_range = end > line_start && start <= line_end_byte;
            if row_in_range {
                let bs = start.saturating_sub(line_start).min(line.len());
                let be = (end - line_start).min(line.len());
                let row_spans = self.highlights_at(row);
                let mut emitted_copy_as: std::collections::HashSet<usize> =
                    std::collections::HashSet::new();
                let mut col: u16 = 0;
                let mut byte_pos: usize = 0;
                for ch in line.chars() {
                    let cw = UnicodeWidthChar::width(ch).unwrap_or(0).max(1) as u16;
                    let ch_byte_end = byte_pos + ch.len_utf8();
                    let in_byte_clip = ch_byte_end > bs && byte_pos < be;
                    if in_byte_clip {
                        let mut unselectable = false;
                        let mut copy_as_hit: Option<(usize, &str)> = None;
                        for (idx, span) in row_spans.iter().enumerate() {
                            if col >= span.col_start && col < span.col_end {
                                if !span.meta.selectable {
                                    unselectable = true;
                                    break;
                                }
                                if let Some(ref s) = span.meta.copy_as {
                                    copy_as_hit = Some((idx, s.as_str()));
                                }
                            }
                        }
                        if !unselectable {
                            if let Some((idx, s)) = copy_as_hit {
                                if emitted_copy_as.insert(idx) {
                                    out.push_str(s);
                                }
                            } else {
                                out.push(ch);
                            }
                        }
                    }
                    col = col.saturating_add(cw);
                    byte_pos += ch.len_utf8();
                }
            }
            // +1 accounts for the `\n` joiner between rows.
            if row + 1 < total_lines && end > line_end_byte + 1 {
                out.push('\n');
            }
            line_start = line_end_byte + 1;
        }
        out
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    // ── Editable buffer API ──────────────────────────────────────────

    /// Call after mutating `source` via `edit_refs()` / `source_mut()`.
    /// Re-projects `source` into `lines` + extmarks:
    ///   - With parser: invalidate cache, re-run `parser.parse(self, source, width)`.
    ///   - Without parser: identity split of `source` into `lines`.
    pub fn sync_after_edit(&mut self, width: u16) {
        if self.parser.is_some() {
            self.last_render = None;
            self.ensure_rendered_at(width);
        } else {
            let new_lines: Vec<String> = self.source.split('\n').map(String::from).collect();
            self.set_all_lines(new_lines);
        }
    }

    /// Map an editable-space byte offset to a display `(row, byte_col)` where
    /// `byte_col` is the byte offset inside `lines[row]`. Companion to
    /// [`display_cursor_pos`], which converts that byte offset to a cell column.
    /// Used by callers that then project through `WrappedLayout` (which works in
    /// byte space).
    pub fn display_byte_pos(&self, cpos: usize) -> (usize, usize) {
        if let Some(maps) = &self.projection_maps {
            let (row, cell) = maps.cursor_pos(&self.source, cpos);
            let line = self.lines.get(row).map(String::as_str).unwrap_or("");
            return (row, crate::text::cell_to_byte(line, cell));
        }
        if self.lines.is_empty() {
            return (0, 0);
        }
        let offsets = crate::text::line_start_offsets(&self.lines);
        let tail = offsets[self.lines.len() - 1] + self.lines[self.lines.len() - 1].len();
        let cpos = cpos.min(tail);
        let line_idx = match offsets.binary_search(&cpos) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        (line_idx, cpos.saturating_sub(offsets[line_idx]))
    }

    /// Map an editable-space byte offset to a display `(row, col)`.
    ///
    /// Uses the parser-written [`ProjectionMaps`](crate::coords::ProjectionMaps)
    /// if present; otherwise walks `lines()` directly (1:1 identity mapping).
    pub fn display_cursor_pos(&self, cpos: usize) -> (usize, usize) {
        if let Some(maps) = &self.projection_maps {
            return maps.cursor_pos(&self.source, cpos);
        }
        if self.lines.is_empty() {
            return (0, 0);
        }
        let offsets = crate::text::line_start_offsets(&self.lines);
        let tail = offsets[self.lines.len() - 1] + self.lines[self.lines.len() - 1].len();
        let cpos = cpos.min(tail);
        let line_idx = match offsets.binary_search(&cpos) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let byte_col = cpos.saturating_sub(offsets[line_idx]);
        let col = crate::text::byte_to_cell(&self.lines[line_idx], byte_col);
        (line_idx, col)
    }

    /// Map a display `(row, col)` back to an editable-space byte offset.
    ///
    /// Uses the parser-written [`ProjectionMaps`](crate::coords::ProjectionMaps)
    /// if present; otherwise walks `lines()` directly (1:1 identity mapping).
    pub fn byte_at_display_pos(&self, row: usize, col: usize) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let row = row.min(self.lines.len() - 1);
        let line = &self.lines[row];
        if let Some(maps) = &self.projection_maps {
            // Clamp `col` to the row's display-char count. `ProjectionMaps`
            // stores one continuous display↔source char stream, so an
            // unclamped `col` would index past this row and resolve into the
            // NEXT line — making click-past-EOL and "preserve screen row"
            // scrolling onto a shorter row silently slip to the wrong byte.
            let col = col.min(line.chars().count());
            return maps.byte_at(&self.source, row, col);
        }
        // Identity path: `cell_to_byte` clamps `cell > line_width` to `line.len()`.
        let offsets = crate::text::line_start_offsets(&self.lines);
        offsets[row] + crate::text::cell_to_byte(line, col)
    }

    /// Set source↔display coord maps. Parsers call this from `parse()` when
    /// their source bytes don't map 1:1 to display chars.
    pub fn set_projection_maps(&mut self, maps: crate::coords::ProjectionMaps) {
        self.projection_maps = Some(maps);
    }

    /// Clear coord maps so subsequent cursor queries fall back to identity.
    pub fn clear_projection_maps(&mut self) {
        self.projection_maps = None;
    }

    /// Read the source↔display coord maps, if any.
    pub fn projection_maps(&self) -> Option<&crate::coords::ProjectionMaps> {
        self.projection_maps.as_ref()
    }

    /// Monotonic counter, bumped on every mutation that affects rendering —
    /// `lines` edits plus decoration changes. Use as a cheap fingerprint for
    /// cache invalidation tied to displayed output.
    pub fn changedtick(&self) -> u64 {
        self.changedtick
    }

    pub fn last_line_edit(&self) -> Option<LineEdit> {
        self.last_line_edit
    }

    // ── Extmark API (the primary surface) ──────────────────────────

    /// Get-or-create a namespace by name.
    pub fn create_namespace(&mut self, name: &str) -> NsId {
        self.extmarks.create_namespace(name)
    }

    /// Place an extmark in `ns`. Returns the mark's id.
    pub fn set_extmark(
        &mut self,
        ns: NsId,
        line: usize,
        col: usize,
        opts: ExtmarkOpts,
    ) -> ExtmarkId {
        let mark = Extmark {
            start_row: line,
            start_col: col,
            end_row: opts.end_row.unwrap_or(line),
            end_col: opts.end_col.unwrap_or(col),
            payload: opts.payload,
            priority: opts.priority,
            right_gravity: opts.right_gravity,
            end_right_gravity: opts.end_right_gravity,
        };
        match opts.id {
            Some(id) => {
                self.extmarks.replace_extmark(ns, id, mark);
                id
            }
            None => self.extmarks.set_extmark(ns, mark),
        }
    }

    /// Clear extmarks in `ns` within `[line_start, line_end)`. Use `0..usize::MAX` for all.
    pub fn clear_namespace(&mut self, ns: NsId, line_start: usize, line_end: usize) {
        self.extmarks.clear_namespace(ns, line_start, line_end);
    }

    /// All extmarks in `ns`, in insertion order.
    pub fn extmarks(&self, ns: NsId) -> Vec<(ExtmarkId, &Extmark)> {
        match self.extmarks.ns(ns) {
            Some(state) => state.extmarks.iter().map(|(id, m)| (*id, m)).collect(),
            None => Vec::new(),
        }
    }

    // ── Convenience wrappers (highlights / decorations / virt_text / marks) ─

    pub fn add_highlight(&mut self, line: usize, col_start: u16, col_end: u16, style: SpanStyle) {
        self.add_highlight_with_meta(line, col_start, col_end, style, SpanMeta::default());
    }

    pub fn add_highlight_with_meta(
        &mut self,
        line: usize,
        col_start: u16,
        col_end: u16,
        style: SpanStyle,
        meta: SpanMeta,
    ) {
        self.set_extmark(
            self.ns_highlights,
            line,
            col_start as usize,
            ExtmarkOpts::highlight(col_end as usize, style, meta),
        );
    }

    /// Like [`Self::add_highlight_with_meta`] but takes an interned [`HlGroup`]
    /// directly, preserving theme-reactivity when copying spans across buffers.
    pub fn add_highlight_group_with_meta(
        &mut self,
        line: usize,
        col_start: u16,
        col_end: u16,
        hl: HlGroup,
        meta: SpanMeta,
    ) {
        self.set_extmark(
            self.ns_highlights,
            line,
            col_start as usize,
            ExtmarkOpts::highlight_group(col_end as usize, hl, meta),
        );
    }

    pub fn clear_highlights(&mut self, start_line: usize, end_line: usize) {
        let ns = self.ns_highlights;
        self.clear_namespace(ns, start_line, end_line);
    }

    pub fn highlights_at(&self, line: usize) -> Vec<Span> {
        let mut out = Vec::new();
        self.highlights_at_into(line, &mut out);
        out
    }

    /// Extend `out` with all Highlight extmarks for `line`, sorted by
    /// (priority, ns-id, insertion order). Reuses the caller's buffer to
    /// avoid one allocation per call in tight render loops.
    pub fn highlights_at_into(&self, line: usize, out: &mut Vec<Span>) {
        // We need to sort by (priority, ns, id) before producing `Span`s, but
        // `Span` doesn't carry those keys. Thread-local scratch keeps the
        // tuple buffer reused across calls without re-allocating per row.
        // Not reentrant: `with_borrow_mut` will panic if this is ever called
        // from inside another `highlights_at_into` (e.g. via a paint hook).
        thread_local! {
            static SCRATCH: std::cell::RefCell<Vec<(u32, u32, u32, Span)>> =
                const { std::cell::RefCell::new(Vec::new()) };
        }
        SCRATCH.with_borrow_mut(|buf| {
            buf.clear();
            // namespaces iterates in NsId order (BTreeMap); marks_at filters by
            // row in O(k) using the secondary index. Priority/ns/id sort below.
            for (ns, state) in self.extmarks.namespaces.iter() {
                for (id, mark) in state.marks_at(line) {
                    if let ExtmarkPayload::Highlight {
                        hl,
                        meta,
                        hl_eol,
                        on_cursor_row,
                        ..
                    } = &mark.payload
                    {
                        buf.push((
                            mark.priority,
                            ns.0,
                            id.0,
                            Span {
                                col_start: mark.start_col as u16,
                                col_end: mark.end_col as u16,
                                hl: *hl,
                                meta: meta.clone(),
                                hl_eol: *hl_eol,
                                on_cursor_row: *on_cursor_row,
                            },
                        ));
                    }
                }
            }
            buf.sort_by_key(|(p, n, i, _)| (*p, *n, *i));
            out.extend(buf.drain(..).map(|(_, _, _, s)| s));
        });
    }

    /// Logical line mapping for `row`, used by gutter providers like `LineNumberGutter`.
    /// Falls through to the per-row decoration's `source_line`; `None` means "use row + 1".
    pub fn source_line_at(&self, row: usize) -> Option<SourceLine> {
        self.decoration_at(row).source_line
    }

    pub fn set_decoration(&mut self, line: usize, decoration: LineDecoration) {
        // One decoration per line: replace any prior mark at this row.
        let ns = self.ns_decorations;
        let to_remove: Vec<ExtmarkId> = self
            .extmarks
            .ns(ns)
            .into_iter()
            .flat_map(|s| s.marks_at(line).map(|(id, _)| id))
            .collect();
        for id in to_remove {
            self.extmarks.del_extmark(ns, id);
        }
        self.set_extmark(ns, line, 0, ExtmarkOpts::decoration(decoration));
        // Decorations affect rendering (gutter widths, pre_formatted wrap policy,
        // bg fills). Bump the tick so caches keyed on it (LineNumberGutter widths,
        // Window wrap layout) invalidate.
        self.changedtick += 1;
        if let Some(edit) = self.last_line_edit.as_mut() {
            if line >= edit.start {
                edit.after_tick = self.changedtick;
            }
        }
    }

    pub fn decoration_at(&self, line: usize) -> &LineDecoration {
        static DEFAULT: LineDecoration = LineDecoration {
            fill_bg: None,
            soft_wrapped: false,
            cell_selectable: false,
            block_selectable: false,
            copy_continuation: false,
            source_text: None,
            source_line: None,
            pre_formatted: false,
        };
        let Some(state) = self.extmarks.ns(self.ns_decorations) else {
            return &DEFAULT;
        };
        for (_id, mark) in state.marks_at(line) {
            if let ExtmarkPayload::Decoration(dec) = &mark.payload {
                return dec;
            }
        }
        &DEFAULT
    }

    /// Set a single virt_text on `line` in `NS_VIRT_TEXT`, replacing any prior.
    pub fn set_virtual_text(&mut self, line: usize, text: String, hl_group: Option<String>) {
        let ns = self.ns_virt_text;
        let to_remove: Vec<ExtmarkId> = self
            .extmarks
            .ns(ns)
            .into_iter()
            .flat_map(|s| s.marks_at(line).map(|(id, _)| id))
            .collect();
        for id in to_remove {
            self.extmarks.del_extmark(ns, id);
        }
        self.set_extmark(ns, line, 0, ExtmarkOpts::virt_text(text, hl_group));
    }

    /// Clear virt_text on `line` in `NS_VIRT_TEXT`.
    pub fn clear_virtual_text(&mut self, line: usize) {
        let ns = self.ns_virt_text;
        let to_remove: Vec<ExtmarkId> = self
            .extmarks
            .ns(ns)
            .into_iter()
            .flat_map(|s| s.marks_at(line).map(|(id, _)| id))
            .collect();
        for id in to_remove {
            self.extmarks.del_extmark(ns, id);
        }
    }

    /// All virt-text entries for `line`, sorted by (priority, ns-id, insertion order).
    pub fn virtual_text_at(&self, line: usize) -> Vec<VirtualText> {
        let mut out = Vec::new();
        self.virtual_text_at_into(line, &mut out);
        out
    }

    pub fn virtual_text_at_into(&self, line: usize, out: &mut Vec<VirtualText>) {
        // See `highlights_at_into` for the SCRATCH design rationale and
        // reentrancy caveat — same pattern, different payload.
        thread_local! {
            static SCRATCH: std::cell::RefCell<Vec<(u32, u32, u32, VirtualText)>> =
                const { std::cell::RefCell::new(Vec::new()) };
        }
        SCRATCH.with_borrow_mut(|buf| {
            buf.clear();
            for (ns, state) in self.extmarks.namespaces.iter() {
                for (id, mark) in state.marks_at(line) {
                    if let ExtmarkPayload::VirtText {
                        text,
                        hl_group,
                        pos,
                    } = &mark.payload
                    {
                        buf.push((
                            mark.priority,
                            ns.0,
                            id.0,
                            VirtualText {
                                col: mark.start_col,
                                text: text.clone(),
                                hl_group: hl_group.clone(),
                                pos: *pos,
                            },
                        ));
                    }
                }
            }
            buf.sort_by_key(|(p, n, i, _)| (*p, *n, *i));
            out.extend(buf.drain(..).map(|(_, _, _, v)| v));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_buf() -> Buffer {
        Buffer::new(BufId(1), BufCreateOpts::default())
    }

    #[test]
    fn new_buffer_has_one_empty_line() {
        let buf = make_buf();
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.get_line(0), Some(""));
    }

    #[test]
    fn set_lines_replaces_range() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["a".into(), "b".into(), "c".into()]);
        buf.set_lines(1, 2, vec!["x".into(), "y".into()]);
        assert_eq!(buf.lines(), &["a", "x", "y", "c"]);
    }

    #[test]
    fn set_lines_clamps_range() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["a".into()]);
        buf.set_lines(0, 100, vec!["replaced".into()]);
        assert_eq!(buf.lines(), &["replaced"]);
    }

    #[test]
    fn ensure_rendered_at_without_parser_is_noop() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["a very long line that would wrap".into()]);
        assert!(!buf.ensure_rendered_at(10));
        assert_eq!(buf.line_count(), 1);
    }

    #[test]
    fn set_all_lines_empty_keeps_one_line() {
        let mut buf = make_buf();
        buf.set_all_lines(vec![]);
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.get_line(0), Some(""));
    }

    #[test]
    fn changedtick_increments() {
        let mut buf = make_buf();
        let t0 = buf.changedtick();
        buf.set_all_lines(vec!["a".into()]);
        assert!(buf.changedtick() > t0);
        let t1 = buf.changedtick();
        buf.set_all_lines(vec!["b".into()]);
        assert!(buf.changedtick() > t1);
    }

    #[test]
    fn virtual_text_lifecycle() {
        let mut buf = make_buf();
        buf.set_virtual_text(0, "ghost".into(), None);
        assert_eq!(buf.virtual_text_at(0).len(), 1);
        assert_eq!(buf.virtual_text_at(0)[0].text, "ghost");
        buf.clear_virtual_text(0);
        assert!(buf.virtual_text_at(0).is_empty());
    }

    #[test]
    fn virtual_text_at_walks_every_namespace_in_nsid_order() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["hi".into()]);
        let ns_a = buf.create_namespace("a");
        let ns_b = buf.create_namespace("b");
        buf.set_extmark(ns_a, 0, 0, ExtmarkOpts::virt_text("from-a".into(), None));
        buf.set_extmark(ns_b, 0, 0, ExtmarkOpts::virt_text("from-b".into(), None));
        let vts = buf.virtual_text_at(0);
        assert_eq!(vts.len(), 2);
        assert_eq!(vts[0].text, "from-a");
        assert_eq!(vts[1].text, "from-b");
    }

    #[test]
    fn text_joins_lines() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["hello".into(), "world".into()]);
        assert_eq!(buf.text(), "hello\nworld");
    }

    #[test]
    fn extract_text_skips_unselectable_spans() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["abXYcd".into()]);
        buf.add_highlight_with_meta(
            0,
            2,
            4,
            SpanStyle::new(),
            SpanMeta {
                selectable: false,
                copy_as: None,
            },
        );
        assert_eq!(buf.extract_text(0, 6), "abcd");
    }

    #[test]
    fn extract_text_substitutes_copy_as_once_per_span() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["abXYcd".into()]);
        buf.add_highlight_with_meta(
            0,
            2,
            4,
            SpanStyle::new(),
            SpanMeta {
                selectable: true,
                copy_as: Some("[snip]".into()),
            },
        );
        assert_eq!(buf.extract_text(0, 6), "ab[snip]cd");
    }

    #[test]
    fn extract_text_joins_rows_with_newline() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["abc".into(), "def".into(), "ghi".into()]);
        assert_eq!(buf.extract_text(0, 11), "abc\ndef\nghi");
        assert_eq!(buf.extract_text(0, 6), "abc\nde");
    }

    #[test]
    fn add_highlight_round_trips_via_extmark() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["hello world".into()]);
        buf.add_highlight(0, 0, 5, SpanStyle::new().bold());
        let spans = buf.highlights_at(0);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].col_start, 0);
        assert_eq!(spans[0].col_end, 5);
        let resolved = crate::theme::Theme::default().resolve(spans[0].hl);
        assert!(resolved.bold);
    }

    #[test]
    fn set_decoration_round_trips_via_extmark() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["a".into()]);
        buf.set_decoration(
            0,
            LineDecoration {
                fill_bg: Some(Color::Blue),
                ..LineDecoration::default()
            },
        );
        assert_eq!(buf.decoration_at(0).fill_bg, Some(Color::Blue));
    }

    #[test]
    fn set_decoration_replaces_prior() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["a".into()]);
        buf.set_decoration(
            0,
            LineDecoration {
                fill_bg: Some(Color::Red),
                ..LineDecoration::default()
            },
        );
        buf.set_decoration(
            0,
            LineDecoration {
                fill_bg: Some(Color::Blue),
                ..LineDecoration::default()
            },
        );
        assert_eq!(buf.decoration_at(0).fill_bg, Some(Color::Blue));
    }

    #[test]
    fn clear_highlights_only_clears_range() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["a".into(), "b".into(), "c".into()]);
        buf.add_highlight(0, 0, 1, SpanStyle::new().bold());
        buf.add_highlight(1, 0, 1, SpanStyle::new().bold());
        buf.add_highlight(2, 0, 1, SpanStyle::new().bold());
        buf.clear_highlights(1, 2);
        assert_eq!(buf.highlights_at(0).len(), 1);
        assert_eq!(buf.highlights_at(1).len(), 0);
        assert_eq!(buf.highlights_at(2).len(), 1);
    }

    #[test]
    fn copy_range_default_is_identity_for_both_outputs() {
        let mut buf = make_buf();
        buf.set_source("hello world".into());
        let out = buf.copy_range(0..5);
        assert_eq!(out.kill_ring, "hello");
        assert_eq!(out.clipboard, "hello");
    }

    #[test]
    fn copy_range_clamps_stale_endpoints_without_panicking() {
        let mut buf = make_buf();
        buf.set_source("abc".into());
        // Endpoints past `source.len()` clamp to len; reversed/empty → default.
        let out = buf.copy_range(100..200);
        assert!(out.is_empty());
        let out = buf.copy_range(0..2);
        assert_eq!(out.kill_ring, "ab");
    }

    #[test]
    fn copy_range_dispatches_to_installed_copier() {
        struct UpperCopier;
        impl BufferCopy for UpperCopier {
            fn copy(&self, _buf: &Buffer, src: &str, range: std::ops::Range<usize>) -> CopyOutput {
                let raw = src[range].to_string();
                CopyOutput {
                    clipboard: raw.to_uppercase(),
                    kill_ring: raw,
                }
            }
        }
        let mut buf = make_buf();
        buf.set_source("hello".into());
        buf.set_copier(std::sync::Arc::new(UpperCopier));
        let out = buf.copy_range(0..5);
        assert_eq!(out.kill_ring, "hello");
        assert_eq!(out.clipboard, "HELLO");
    }

    #[test]
    fn sync_clipboard_uses_copier_when_buffer_untouched() {
        struct UpperCopier;
        impl BufferCopy for UpperCopier {
            fn copy(&self, _buf: &Buffer, src: &str, range: std::ops::Range<usize>) -> CopyOutput {
                let raw = src[range].to_string();
                CopyOutput {
                    clipboard: raw.to_uppercase(),
                    kill_ring: raw,
                }
            }
        }
        let mut buf = make_buf();
        buf.set_source("hello".into());
        buf.set_copier(std::sync::Arc::new(UpperCopier));

        let mut clipboard = crate::Clipboard::null();
        clipboard
            .kill_ring
            .set_with_source("hello".into(), false, 0, 5);
        buf.sync_clipboard_from_kill_ring(&mut clipboard);
        assert_eq!(clipboard.kill_ring.last_clipboard_write(), Some("HELLO"));
    }

    #[test]
    fn sync_clipboard_falls_back_to_kill_ring_when_buffer_mutated() {
        struct UpperCopier;
        impl BufferCopy for UpperCopier {
            fn copy(&self, _buf: &Buffer, src: &str, range: std::ops::Range<usize>) -> CopyOutput {
                let raw = src[range].to_string();
                CopyOutput {
                    clipboard: raw.to_uppercase(),
                    kill_ring: raw,
                }
            }
        }
        let mut buf = make_buf();
        buf.set_source("hello".into());
        buf.set_copier(std::sync::Arc::new(UpperCopier));

        let mut clipboard = crate::Clipboard::null();
        // Simulate vim x on the last char: yank "o" at 4..5 then delete it.
        clipboard.kill_ring.set_with_source("o".into(), false, 4, 5);
        // Mutate buffer so the range no longer contains "o".
        buf.set_source("hell".into());
        buf.sync_clipboard_from_kill_ring(&mut clipboard);
        // Should fall back to kill-ring text, not the shifted/wrong buffer text.
        assert_eq!(clipboard.kill_ring.last_clipboard_write(), Some("o"));
    }

    #[test]
    fn sync_clipboard_falls_back_when_range_collapses_after_delete() {
        let mut buf = make_buf();
        buf.set_source("hello".into());

        let mut clipboard = crate::Clipboard::null();
        clipboard.kill_ring.set_with_source("o".into(), false, 4, 5);
        // Delete the last character: buffer shrinks so range 4..5 is empty.
        buf.set_source("hell".into());
        buf.sync_clipboard_from_kill_ring(&mut clipboard);
        assert_eq!(clipboard.kill_ring.last_clipboard_write(), Some("o"));
    }

    /// Property test: stale byte offsets surviving an edit must never panic
    /// the public slicing/mutation APIs.
    #[test]
    fn stale_offsets_never_panic_public_slicing() {
        type Mutate = dyn Fn(&mut crate::attached::AttachedTextMut<'_>);
        let scenarios: &[(&str, &Mutate)] = &[
            ("日本語hello", &|t| {
                t.insert(0, '🦀');
            }),
            ("日本語hello", &|t| {
                t.clear();
            }),
            ("a🦀b日c", &|t| {
                let p = t.len();
                t.insert_str(p, "XYZ");
            }),
            ("a🦀b日c", &|t| {
                t.install(String::from("z"), Vec::new());
            }),
        ];

        for (initial, mutate) in scenarios {
            let mut buf = make_buf();
            buf.set_source((*initial).to_string());

            let len = buf.source().len();
            let captured: Vec<usize> = (0..=len + 4).collect();

            // Mutate directly — `set_source` would normalize and hide the issue.
            // Build a transient text wrapper just to apply the mutation through
            // the same in-place path production code uses.
            {
                let mut t = buf.text_mut();
                mutate(&mut t);
            }

            for &a in &captured {
                for &b in &captured {
                    let _ = buf.copy_range(a..b);
                    let _ = crate::text::slice(buf.source(), a..b);

                    let mut s = buf.source().to_string();
                    crate::text::replace_range(&mut s, a..b, "");
                    let mut s = buf.source().to_string();
                    crate::text::replace_range(&mut s, a..b, "X");

                    let mut kr = crate::kill_ring::KillRing::new();
                    kr.kill("seed".into());
                    kr.kill("seed2".into());
                    kr.set_with_source("payload".into(), false, a, b);
                    let mut bs = buf.source().to_string();
                    let mut ids = Vec::new();
                    let mut bsmut = crate::attached::AttachedTextMut::new(&mut bs, &mut ids);
                    let _ = kr.yank_pop(&mut bsmut);

                    let mut clone = make_buf();
                    clone.set_source(buf.source().to_string());
                    clone.text_mut().replace_range(a..b, "");
                }
            }
        }
    }

    #[test]
    fn set_all_lines_clears_extmarks() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["a".into(), "b".into()]);
        buf.add_highlight(0, 0, 1, SpanStyle::new().bold());
        buf.set_decoration(
            1,
            LineDecoration {
                fill_bg: Some(Color::Blue),
                ..LineDecoration::default()
            },
        );
        buf.set_all_lines(vec!["x".into()]);
        assert_eq!(buf.highlights_at(0).len(), 0);
        assert_eq!(buf.decoration_at(0).fill_bg, None);
    }

    #[test]
    fn custom_namespace_highlights_surface_alongside_default() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["text".into()]);
        let ns = buf.create_namespace("syntax");
        buf.set_extmark(
            ns,
            0,
            0,
            ExtmarkOpts::highlight(4, SpanStyle::new().fg(Color::Red), SpanMeta::default()),
        );
        assert_eq!(buf.highlights_at(0).len(), 1);
        assert_eq!(buf.extmarks(ns).len(), 1);
        buf.clear_namespace(ns, 0, usize::MAX);
        assert_eq!(buf.extmarks(ns).len(), 0);
        assert_eq!(buf.highlights_at(0).len(), 0);
    }

    #[test]
    fn clear_namespace_only_clears_target() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["a".into()]);
        let ns_a = buf.create_namespace("a");
        let ns_b = buf.create_namespace("b");
        buf.set_extmark(
            ns_a,
            0,
            0,
            ExtmarkOpts::highlight(1, SpanStyle::new().bold(), SpanMeta::default()),
        );
        buf.set_extmark(
            ns_b,
            0,
            0,
            ExtmarkOpts::highlight(1, SpanStyle::new().bold(), SpanMeta::default()),
        );
        buf.clear_namespace(ns_a, 0, usize::MAX);
        assert_eq!(buf.extmarks(ns_a).len(), 0);
        assert_eq!(buf.extmarks(ns_b).len(), 1);
    }

    struct StubParser {
        calls: std::sync::Mutex<Vec<(String, u16)>>,
        attach_calls: std::sync::Mutex<u32>,
    }

    impl StubParser {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::Mutex::new(Vec::new()),
                attach_calls: std::sync::Mutex::new(0),
            })
        }

        fn call_log(&self) -> Vec<(String, u16)> {
            self.calls.lock().unwrap().clone()
        }

        fn attach_count(&self) -> u32 {
            *self.attach_calls.lock().unwrap()
        }
    }

    impl BufferParser for StubParser {
        fn parse(&self, buf: &mut Buffer, source: &str, width: u16) {
            self.calls.lock().unwrap().push((source.to_string(), width));
            buf.set_all_lines(vec![format!("{source}@{width}")]);
        }

        fn on_attach(&self, _buf: &mut Buffer) {
            *self.attach_calls.lock().unwrap() += 1;
        }
    }

    #[test]
    fn parser_runs_once_per_source_width() {
        let p = StubParser::new();
        let mut buf = make_buf().attach(p.clone());
        buf.set_source("x".into());
        assert!(buf.ensure_rendered_at(10));
        assert!(!buf.ensure_rendered_at(10));
        assert!(buf.ensure_rendered_at(20));
        buf.set_source("y".into());
        assert!(buf.ensure_rendered_at(20));
        assert_eq!(
            p.call_log(),
            vec![
                ("x".to_string(), 10),
                ("x".to_string(), 20),
                ("y".to_string(), 20),
            ]
        );
        assert_eq!(buf.get_line(0), Some("y@20"));
    }

    #[test]
    fn setting_same_source_does_not_re_parse() {
        let p = StubParser::new();
        let mut buf = make_buf().attach(p.clone());
        buf.set_source("abc".into());
        buf.ensure_rendered_at(10);
        buf.set_source("abc".into());
        assert!(!buf.ensure_rendered_at(10));
        assert_eq!(p.call_log().len(), 1);
    }

    #[test]
    fn attaching_parser_invalidates_render_cache_and_fires_on_attach() {
        let first = StubParser::new();
        let mut buf = make_buf().attach(first.clone());
        assert_eq!(first.attach_count(), 1);
        buf.set_source("s".into());
        buf.ensure_rendered_at(10);
        let second = StubParser::new();
        buf.set_parser(second.clone());
        assert_eq!(second.attach_count(), 1);
        assert!(buf.ensure_rendered_at(10));
        assert_eq!(second.call_log().len(), 1);
    }

    #[test]
    fn byte_at_display_pos_clamps_col_to_row_width_under_projection() {
        // Source "aaa\nbbb" (7 chars, 7 bytes); display char stream "aaa\nbbb"
        // is one continuous run in the ProjectionMaps tables. Without per-row
        // clamping, `byte_at(0, 5)` would index past row 0 (3 chars) into row
        // 1's chars and resolve to a byte inside "bbb" — wrong row. With the
        // clamp, the column collapses to the row's display-char count.
        let mut buf = make_buf();
        buf.set_source("aaa\nbbb".into());
        buf.set_all_lines(vec!["aaa".into(), "bbb".into()]);
        let s2d: Vec<usize> = (0..=7).collect();
        let d2s: Vec<usize> = (0..=7).collect();
        buf.set_projection_maps(crate::coords::ProjectionMaps {
            source_char_to_display_char: s2d,
            display_char_to_source_char: d2s,
            row_offsets: vec![0, 4],
        });
        // Click well past EOL on row 0 must land at end of "aaa", not in "bbb".
        assert_eq!(buf.byte_at_display_pos(0, 99), 3);
        // Click within row 0 still resolves normally.
        assert_eq!(buf.byte_at_display_pos(0, 1), 1);
        // Click within row 1.
        assert_eq!(buf.byte_at_display_pos(1, 1), 5);
    }
}
