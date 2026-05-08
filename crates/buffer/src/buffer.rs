//! Buffer — lines + namespaced extmarks.
//!
//! Mirrors `nvim_buf_set_extmark`: a `Buffer` holds text lines plus
//! `Extmark`s grouped into namespaces. Highlights, decorations, and
//! virtual text are all extmarks queried per-line at render time.

use crate::attachment::AttachmentId;
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

/// Smallest id minted by plugin-side `smelt.buf.create`. Keeps Lua
/// buffers in a disjoint range from Rust's sequential allocator.
pub const LUA_BUF_ID_BASE: u64 = 1 << 32;

/// Parser attached to a `Buffer`. Rebuilds lines, extmarks, and
/// decorations from a source string whenever `(source_tick, width)`
/// changes. `on_attach` fires once at installation for namespace setup.
pub trait BufferParser: Send + Sync {
    /// Rebuild the buffer's lines / extmarks / decorations from
    /// `source` at the given render `width`.
    fn parse(&self, buf: &mut Buffer, source: &str, width: u16);

    /// Called once when the parser is installed. Default no-op.
    fn on_attach(&self, _buf: &mut Buffer) {}
}

/// Process-global namespace id. Stable for the process lifetime;
/// the same name always yields the same id across every `Buffer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NsId(pub u32);

/// Idempotent name → id minter. Same `name` always returns the same id.
pub fn create_namespace(name: &str) -> NsId {
    use std::sync::{OnceLock, RwLock};
    static REG: OnceLock<RwLock<NamespaceRegistry>> = OnceLock::new();
    let reg = REG.get_or_init(|| RwLock::new(NamespaceRegistry::default()));
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

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LineDecoration {
    pub gutter_bg: Option<Color>,
    pub fill_bg: Option<Color>,
    pub fill_right_margin: u16,
    pub soft_wrapped: bool,
    pub source_text: Option<String>,
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

    fn clear_all(&mut self) {
        self.extmarks.clear();
        self.by_row.clear();
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
    /// Interned at construction so convenience methods skip a hashmap lookup.
    ns_highlights: NsId,
    ns_decorations: NsId,
    ns_virt_text: NsId,
    parser: Option<Arc<dyn BufferParser>>,
    source: String,
    source_tick: u64,
    last_render: Option<(u64, u16)>,
    pub history: UndoHistory,
    pub attachment_ids: Vec<AttachmentId>,
    pub readonly: bool,
    selection: Vec<SelectionRange>,
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
            ns_highlights,
            ns_decorations,
            ns_virt_text,
            parser: None,
            source: String::new(),
            source_tick: 0,
            last_render: None,
            history: UndoHistory::default(),
            attachment_ids: Vec::new(),
            readonly: false,
            selection: Vec::new(),
        }
    }

    /// Override the per-row visual-mode selection. Empty `ranges` clears
    /// the override; `Window::render` then derives selection from its own state.
    pub fn set_selection(&mut self, ranges: Vec<SelectionRange>) {
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

    /// Update the source driving the parser. No-op if source is unchanged.
    pub fn set_source(&mut self, source: String) {
        if source == self.source {
            return;
        }
        self.source = source;
        self.source_tick = self.source_tick.wrapping_add(1);
    }

    /// Re-run the parser if `(source_tick, width)` differs from the last call.
    /// Returns `true` when a parse actually happened.
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
        // Seed with one empty line so parsers can start from row 0.
        let n = self.lines.len();
        if n > 1 || (n == 1 && !self.lines[0].is_empty()) {
            self.set_lines(0, n, vec![String::new()]);
        }
        for state in self.extmarks.namespaces.values_mut() {
            state.clear_all();
        }
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
        let lines = Arc::make_mut(&mut self.lines);
        lines.splice(start..end, replacement);
        if lines.is_empty() {
            lines.push(String::new());
        }
        // Clear extmarks in the replaced range (wholesale line replacement
        // drops all marks in the slice, mirroring nvim's behavior).
        for ns in [self.ns_highlights, self.ns_decorations, self.ns_virt_text] {
            self.extmarks.clear_namespace(ns, start, end);
        }
        self.selection.retain(|r| r.line < start || r.line >= end);
        self.changedtick += 1;
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

    #[cfg(test)]
    pub fn changedtick(&self) -> u64 {
        self.changedtick
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
                        hl, meta, hl_eol, ..
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
                            },
                        ));
                    }
                }
            }
            buf.sort_by_key(|(p, n, i, _)| (*p, *n, *i));
            out.extend(buf.drain(..).map(|(_, _, _, s)| s));
        });
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
    }

    pub fn decoration_at(&self, line: usize) -> &LineDecoration {
        static DEFAULT: LineDecoration = LineDecoration {
            gutter_bg: None,
            fill_bg: None,
            fill_right_margin: 0,
            soft_wrapped: false,
            source_text: None,
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
}
