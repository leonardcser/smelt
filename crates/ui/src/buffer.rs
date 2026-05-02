//! Buffer — lines + namespaced extmarks.
//!
//! The data layer mirrors `nvim_buf_set_extmark`: a `Buffer` is a
//! sequence of text lines plus a single store of `Extmark`s grouped
//! into `Namespace`s. Highlight spans, line decorations, and virtual
//! text are all extmarks tagged by namespace — one storage shape,
//! queried per-line at render time.
//!
//! The convenience methods `add_highlight` and `set_decoration` create
//! extmarks in well-known namespaces (`Buffer::NS_HIGHLIGHTS`,
//! `NS_DECORATIONS`). Code that wants nvim's full extmark ergonomics
//! (custom namespace, `clear_namespace`, IDs) calls `create_namespace`
//! plus `set_extmark` directly. Virt-text always goes through the
//! latter shape (`ExtmarkOpts::virt_text`); production has no
//! virt-text convenience method.

use crate::{AttachmentId, BufId};
use crossterm::style::Color;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// Parser attached to a `Buffer` to maintain its lines, extmarks, and
/// decorations from a `source` string. Host crates implement this
/// trait for each "content kind" a buffer can display (markdown,
/// bash, syntax-highlighted file, inline diff, plain wrap, …); the
/// `ui` crate knows nothing about any specific format and calls back
/// through the lifecycle hooks below when the buffer's source or
/// render width changes.
///
/// `parse` is the only required hook today; it has wholesale-rebuild
/// semantics. The optional `on_attach` lifecycle hook lets a parser
/// install namespaces or seed initial state the moment the parser is
/// installed on a Buffer (mirrors nvim's `nvim_buf_attach`). Future
/// hooks (`on_change`, `on_render`) will let parsers respond
/// incrementally to line edits and width changes; for now, `parse`
/// runs whenever `(source_tick, width)` differs from the last call.
pub trait BufferParser: Send + Sync {
    /// Rebuild the buffer's lines / extmarks / decorations from
    /// `source` at the given render `width`. Free to call any mutator
    /// on `buf` (`set_all_lines`, `add_highlight`, `set_decoration`,
    /// …). The buffer's `source` is read-only from the parser's
    /// point of view and lives on `Buffer` untouched across calls.
    fn parse(&self, buf: &mut Buffer, source: &str, width: u16);

    /// Called once when the parser is installed via
    /// [`Buffer::attach`] / [`Buffer::set_parser`]. Default no-op;
    /// override to register custom namespaces, seed marks, or run
    /// any one-time setup that doesn't depend on `source` or width.
    fn on_attach(&self, _buf: &mut Buffer) {}
}

/// Identifier returned by `Buffer::create_namespace`. Stable for the
/// lifetime of the Buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NsId(pub u32);

/// Identifier returned by `Buffer::set_extmark`. Unique within a
/// namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExtmarkId(pub u32);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpanMeta {
    pub selectable: bool,
    pub copy_as: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LineDecoration {
    pub gutter_bg: Option<Color>,
    pub fill_bg: Option<Color>,
    pub fill_right_margin: u16,
    pub soft_wrapped: bool,
    pub source_text: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpanStyle {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
}

impl SpanStyle {
    pub fn fg(color: Color) -> Self {
        Self {
            fg: Some(color),
            ..Default::default()
        }
    }

    pub fn dim() -> Self {
        Self {
            dim: true,
            ..Default::default()
        }
    }

    pub fn bold() -> Self {
        Self {
            bold: true,
            ..Default::default()
        }
    }

    pub fn bg(color: Color) -> Self {
        Self {
            bg: Some(color),
            ..Default::default()
        }
    }
}

/// Materialized highlight span for one line. Derived on demand from
/// extmarks in `NS_HIGHLIGHTS` (or any namespace whose payload is
/// `ExtmarkPayload::Highlight`); split at line boundaries when an
/// extmark spans multiple rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub col_start: u16,
    pub col_end: u16,
    pub style: SpanStyle,
    pub meta: SpanMeta,
}

/// One-line virtual text overlay. Derived on demand from extmarks in
/// `NS_VIRT_TEXT` (or any namespace whose payload is
/// `ExtmarkPayload::VirtText`).
#[derive(Clone, Debug)]
pub struct VirtualText {
    pub line: usize,
    pub col: usize,
    pub text: String,
    pub hl_group: Option<String>,
}

#[derive(Default)]
pub struct BufCreateOpts {}

// ─── Extmark model ─────────────────────────────────────────────────

/// One extmark — a positional anchor with a payload. Lives in a
/// namespace; addressable by `(NsId, ExtmarkId)`.
#[derive(Clone, Debug)]
pub struct Extmark {
    pub start_row: usize,
    pub start_col: usize,
    pub end_row: usize,
    pub end_col: usize,
    pub payload: ExtmarkPayload,
}

/// Payload carried by an extmark. Each variant maps onto one of the
/// public per-line getters (`highlights_at`, `decoration_at`,
/// `virtual_text_at`).
#[derive(Clone, Debug)]
pub enum ExtmarkPayload {
    Highlight {
        style: SpanStyle,
        meta: SpanMeta,
    },
    Decoration(LineDecoration),
    VirtText {
        text: String,
        hl_group: Option<String>,
    },
}

/// `set_extmark` opts. `end_row`/`end_col` default to the start
/// position (a point mark); supply both to span a range.
#[derive(Clone, Debug)]
pub struct ExtmarkOpts {
    pub end_row: Option<usize>,
    pub end_col: Option<usize>,
    pub payload: ExtmarkPayload,
}

impl ExtmarkOpts {
    pub fn highlight(end_col: usize, style: SpanStyle, meta: SpanMeta) -> Self {
        Self {
            end_row: None,
            end_col: Some(end_col),
            payload: ExtmarkPayload::Highlight { style, meta },
        }
    }

    pub fn decoration(dec: LineDecoration) -> Self {
        Self {
            end_row: None,
            end_col: None,
            payload: ExtmarkPayload::Decoration(dec),
        }
    }

    pub fn virt_text(text: String, hl_group: Option<String>) -> Self {
        Self {
            end_row: None,
            end_col: None,
            payload: ExtmarkPayload::VirtText { text, hl_group },
        }
    }
}

#[derive(Default, Clone)]
struct NamespaceState {
    extmarks: BTreeMap<ExtmarkId, Extmark>,
    next_id: u32,
}

#[derive(Default, Clone)]
struct ExtmarkStore {
    namespaces: HashMap<NsId, NamespaceState>,
    name_to_id: HashMap<String, NsId>,
    next_ns: u32,
}

impl ExtmarkStore {
    fn create_namespace(&mut self, name: &str) -> NsId {
        if let Some(id) = self.name_to_id.get(name) {
            return *id;
        }
        let id = NsId(self.next_ns);
        self.next_ns += 1;
        self.namespaces.insert(id, NamespaceState::default());
        self.name_to_id.insert(name.to_string(), id);
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
        state.extmarks.insert(id, mark);
        id
    }

    fn del_extmark(&mut self, ns: NsId, id: ExtmarkId) -> Option<Extmark> {
        self.namespaces.get_mut(&ns)?.extmarks.remove(&id)
    }

    fn clear_namespace(&mut self, ns: NsId, line_start: usize, line_end: usize) {
        let Some(state) = self.namespaces.get_mut(&ns) else {
            return;
        };
        state
            .extmarks
            .retain(|_, m| !overlaps_lines(m, line_start, line_end));
    }
}

fn overlaps_lines(m: &Extmark, line_start: usize, line_end: usize) -> bool {
    let m_end = m.end_row.max(m.start_row);
    m.start_row < line_end && m_end >= line_start
}

#[derive(Clone)]
pub struct Buffer {
    pub(crate) id: BufId,
    /// `Arc`-wrapped so `Buffer::clone()` and sync-to-view become
    /// refcount bumps; mutators use `Arc::make_mut` which only deep-
    /// copies when the Arc is actually shared.
    lines: Arc<Vec<String>>,
    extmarks: ExtmarkStore,
    /// Bumped on lines mutation.
    changedtick: u64,
    /// Well-known namespace ids — interned at construction so the
    /// convenience methods (`add_highlight`, `set_decoration`, …)
    /// don't pay a hashmap lookup per call.
    ns_highlights: NsId,
    ns_decorations: NsId,
    ns_virt_text: NsId,
    /// When set, `source` drives the visible lines: the parser
    /// re-runs `parse` into this buffer lazily when
    /// `ensure_rendered_at` is called with a different
    /// `(source_tick, width)` than the last call.
    parser: Option<Arc<dyn BufferParser>>,
    source: String,
    source_tick: u64,
    last_render: Option<(u64, u16)>,
    /// Edit-state fields absorbed from `EditBuffer` (P1.a-tail).
    /// Per-buffer undo/redo stack. `None` capacity disables undo
    /// (used for readonly buffers).
    pub history: crate::undo::UndoHistory,
    /// Attachment markers inside the buffer text.
    pub attachment_ids: Vec<AttachmentId>,
    /// Whether this buffer can be edited. Windows check this before
    /// running any edit-producing operation.
    pub readonly: bool,
}

impl Buffer {
    /// Default namespace name for highlight extmarks created via
    /// `add_highlight` / `add_highlight_with_meta`.
    pub const NS_HIGHLIGHTS: &'static str = "buffer.highlights";
    /// Default namespace name for line decorations created via
    /// `set_decoration`.
    pub const NS_DECORATIONS: &'static str = "buffer.decorations";
    /// Default namespace name for virtual text created via the
    /// test-only `set_virtual_text` helper. Production virt-text is
    /// stored under per-feature namespaces (`completer`, `GhostText`,
    /// …) reached through `set_extmark` + `ExtmarkOpts::virt_text`.
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
            history: crate::undo::UndoHistory::default(),
            attachment_ids: Vec::new(),
            readonly: false,
        }
    }

    /// Attach a parser. Fires `BufferParser::on_attach` once,
    /// invalidates the render cache so the next `ensure_rendered_at`
    /// call re-runs `parse` from the current `source`. Replaces any
    /// prior parser.
    pub fn set_parser(&mut self, parser: Arc<dyn BufferParser>) {
        parser.on_attach(self);
        self.parser = Some(parser);
        self.last_render = None;
    }

    /// Builder form of `set_parser`. Mirrors `nvim_buf_attach` — the
    /// returned Buffer has the parser installed and `on_attach`
    /// already fired.
    pub fn attach(mut self, parser: Arc<dyn BufferParser>) -> Self {
        self.set_parser(parser);
        self
    }

    /// Update the source driving the parser. The next
    /// `ensure_rendered_at` will re-run `parse`; without a parser
    /// attached, the source is held but never consulted.
    pub fn set_source(&mut self, source: String) {
        if source == self.source {
            return;
        }
        self.source = source;
        self.source_tick = self.source_tick.wrapping_add(1);
    }

    /// Re-run the parser if `(source, width)` differs from the last
    /// call. No-op without a parser or when nothing changed.
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
        // Clear extmarks whose anchor falls in the replaced range.
        // Mirrors nvim's behavior: marks track edits, but a wholesale
        // line replacement is treated as "everything in this slice
        // is gone."
        for ns in [self.ns_highlights, self.ns_decorations, self.ns_virt_text] {
            self.extmarks.clear_namespace(ns, start, end);
        }
        self.changedtick += 1;
    }

    pub fn set_all_lines(&mut self, lines: Vec<String>) {
        let new_lines = if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        };
        self.lines = Arc::new(new_lines);
        // Wholesale replacement: drop every extmark in the well-known
        // namespaces. (Custom namespaces persist; their owners decide
        // when to clear.)
        for ns in [self.ns_highlights, self.ns_decorations, self.ns_virt_text] {
            self.extmarks.clear_namespace(ns, 0, usize::MAX);
        }
        self.changedtick += 1;
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    #[cfg(test)]
    pub fn changedtick(&self) -> u64 {
        self.changedtick
    }

    // ── Extmark API (the primary surface) ──────────────────────────

    /// Get-or-create a namespace by name. Same `name` always returns
    /// the same `NsId` for the lifetime of the Buffer.
    pub fn create_namespace(&mut self, name: &str) -> NsId {
        self.extmarks.create_namespace(name)
    }

    /// Place an extmark in `ns`. Returns the new mark's id.
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
        };
        self.extmarks.set_extmark(ns, mark)
    }

    /// Clear every extmark in `ns` whose anchor lies within
    /// `[line_start, line_end)`. Pass `0..usize::MAX` to clear the
    /// whole namespace.
    pub fn clear_namespace(&mut self, ns: NsId, line_start: usize, line_end: usize) {
        self.extmarks.clear_namespace(ns, line_start, line_end);
    }

    /// Iterate every extmark in `ns`, in insertion order.
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

    pub fn clear_highlights(&mut self, start_line: usize, end_line: usize) {
        let ns = self.ns_highlights;
        self.clear_namespace(ns, start_line, end_line);
    }

    pub fn highlights_at(&self, line: usize) -> Vec<Span> {
        // Walk every namespace whose extmarks carry highlight payloads,
        // not just `ns_highlights`. Cross-namespace ordering is
        // namespace-id ascending so later-created namespaces (e.g. the
        // transcript's `selection` namespace registered after
        // `ns_highlights`) paint on top of earlier ones. Within a
        // namespace, BTreeMap iteration is by extmark id — insertion
        // order — so spans from a single source paint in registration
        // order. Highlight extmarks today are single-row (matching the
        // nvim convention where line-spanning highlights are emitted
        // per-row by the parser); end-row is recorded but not yet
        // split here.
        let mut ns_ids: Vec<NsId> = self.extmarks.namespaces.keys().copied().collect();
        ns_ids.sort_by_key(|n| n.0);
        let mut out = Vec::new();
        for ns in ns_ids {
            let Some(state) = self.extmarks.ns(ns) else {
                continue;
            };
            for mark in state.extmarks.values() {
                if mark.start_row != line {
                    continue;
                }
                if let ExtmarkPayload::Highlight { style, meta } = &mark.payload {
                    out.push(Span {
                        col_start: mark.start_col as u16,
                        col_end: mark.end_col as u16,
                        style: style.clone(),
                        meta: meta.clone(),
                    });
                }
            }
        }
        out
    }

    pub fn set_decoration(&mut self, line: usize, decoration: LineDecoration) {
        // One decoration per line: clear any prior at this row before
        // writing the new one.
        let ns = self.ns_decorations;
        let to_remove: Vec<ExtmarkId> = self
            .extmarks(ns)
            .into_iter()
            .filter(|(_, m)| m.start_row == line)
            .map(|(id, _)| id)
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
        for mark in state.extmarks.values() {
            if mark.start_row != line {
                continue;
            }
            if let ExtmarkPayload::Decoration(dec) = &mark.payload {
                return dec;
            }
        }
        &DEFAULT
    }

    #[cfg(test)]
    pub(crate) fn set_virtual_text(&mut self, line: usize, text: String, hl_group: Option<String>) {
        // One virt_text per line in the convenience namespace.
        let ns = self.ns_virt_text;
        let to_remove: Vec<ExtmarkId> = self
            .extmarks(ns)
            .into_iter()
            .filter(|(_, m)| m.start_row == line)
            .map(|(id, _)| id)
            .collect();
        for id in to_remove {
            self.extmarks.del_extmark(ns, id);
        }
        self.set_extmark(ns, line, 0, ExtmarkOpts::virt_text(text, hl_group));
    }

    #[cfg(test)]
    pub(crate) fn clear_virtual_text(&mut self, line: usize) {
        let ns = self.ns_virt_text;
        let to_remove: Vec<ExtmarkId> = self
            .extmarks(ns)
            .into_iter()
            .filter(|(_, m)| m.start_row == line)
            .map(|(id, _)| id)
            .collect();
        for id in to_remove {
            self.extmarks.del_extmark(ns, id);
        }
    }

    /// Walk every namespace whose extmarks carry virt-text payloads
    /// (not just `ns_virt_text`). Cross-namespace ordering is namespace-id
    /// ascending so later-created namespaces paint after earlier ones;
    /// within a namespace, BTreeMap iteration is by extmark id —
    /// insertion order — so virt_texts from a single source paint in
    /// registration order. Mirrors the `highlights_at` precedent.
    pub(crate) fn virtual_text_at(&self, line: usize) -> Vec<VirtualText> {
        let mut ns_ids: Vec<NsId> = self.extmarks.namespaces.keys().copied().collect();
        ns_ids.sort_by_key(|n| n.0);
        let mut out = Vec::new();
        for ns in ns_ids {
            let Some(state) = self.extmarks.ns(ns) else {
                continue;
            };
            for mark in state.extmarks.values() {
                if mark.start_row != line {
                    continue;
                }
                if let ExtmarkPayload::VirtText { text, hl_group } = &mark.payload {
                    out.push(VirtualText {
                        line: mark.start_row,
                        col: mark.start_col,
                        text: text.clone(),
                        hl_group: hl_group.clone(),
                    });
                }
            }
        }
        out
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
        // Two namespaces both anchor virt_text on row 0; the
        // later-registered namespace appears after the earlier one in
        // the returned Vec — same paint-order rule as `highlights_at`.
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
    fn add_highlight_round_trips_via_extmark() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["hello world".into()]);
        buf.add_highlight(0, 0, 5, SpanStyle::bold());
        let spans = buf.highlights_at(0);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].col_start, 0);
        assert_eq!(spans[0].col_end, 5);
        assert!(spans[0].style.bold);
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
        buf.add_highlight(0, 0, 1, SpanStyle::bold());
        buf.add_highlight(1, 0, 1, SpanStyle::bold());
        buf.add_highlight(2, 0, 1, SpanStyle::bold());
        buf.clear_highlights(1, 2);
        assert_eq!(buf.highlights_at(0).len(), 1);
        assert_eq!(buf.highlights_at(1).len(), 0);
        assert_eq!(buf.highlights_at(2).len(), 1);
    }

    #[test]
    fn set_all_lines_clears_extmarks() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["a".into(), "b".into()]);
        buf.add_highlight(0, 0, 1, SpanStyle::bold());
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
            ExtmarkOpts::highlight(4, SpanStyle::fg(Color::Red), SpanMeta::default()),
        );
        // Highlight payloads in any namespace are visible to
        // `highlights_at` so parsers / selection / search can
        // partition decoration into independent namespaces while
        // sharing the same paint pass.
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
            ExtmarkOpts::highlight(1, SpanStyle::bold(), SpanMeta::default()),
        );
        buf.set_extmark(
            ns_b,
            0,
            0,
            ExtmarkOpts::highlight(1, SpanStyle::bold(), SpanMeta::default()),
        );
        buf.clear_namespace(ns_a, 0, usize::MAX);
        assert_eq!(buf.extmarks(ns_a).len(), 0);
        assert_eq!(buf.extmarks(ns_b).len(), 1);
    }

    /// Drives [`Buffer::ensure_rendered_at`] with a stub parser so
    /// we can assert the caching + re-render semantics without
    /// pulling the full markdown pipeline into the ui crate.
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
