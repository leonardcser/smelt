use std::{
    borrow::Cow,
    cell::{OnceCell, RefCell},
    collections::VecDeque,
    hash::{Hash, Hasher},
    ops::Range,
    rc::Rc,
    sync::Arc,
};

use super::temp_rows::{apply_temp_decoration, emit_buffer_row_clipped};
use smelt_core::content::builder::{display_width, wrapped_segments, LineBuilder};
use smelt_core::content::code_block::{measure_code_block, parse_code_block};
use smelt_core::content::highlight::{
    emit_inline_spans, inline_spans_width, measure_markdown_table_with_options,
    parse_inline_spans_with_options, render_code_block, render_markdown_table_with_options,
    wrap_inline_spans, InlineOptions, InlineSpan, InlineStyle,
};
use smelt_core::content::inline_line::BreakPolicy;
use smelt_core::content::markdown_ir::{
    markdown_nodes_retained_bytes, parse_markdown_with_options, MarkdownLine, MarkdownNode,
    MarkdownTextKind,
};
use smelt_core::content::{is_markdown_list_item, split_markdown_list_prefix};
use smelt_core::theme::intern;
use smelt_core::transcript_content::{
    ContentId, ContentRead, ContentTextWindow, TranscriptContent,
};

pub fn render_markdown_inner(
    out: &mut LineBuilder,
    content: &str,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
) -> u16 {
    render_markdown_inner_with_options(
        out,
        content,
        width,
        indent,
        dim,
        bctx,
        &InlineOptions::default(),
    )
}

pub fn render_markdown_inner_with_options(
    out: &mut LineBuilder,
    content: &str,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:markdown");
    let block = parse_markdown_cached(content, inline_options);
    render_markdown_block(
        out,
        block.as_doc(),
        width,
        indent,
        dim,
        bctx,
        inline_options,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn render_markdown_inner_range_with_options(
    out: &mut LineBuilder,
    content: &str,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
    row_start: usize,
    row_count: usize,
) -> u16 {
    if row_count == 0 {
        return 0;
    }
    let _perf = smelt_perf::perf::begin("render:markdown:range");
    let before = out.line_count();
    let block = parse_markdown_cached(content, inline_options);
    render_markdown_block(
        out,
        block.as_doc(),
        width,
        indent,
        dim,
        bctx,
        inline_options,
        Some(RowClip {
            start: row_start,
            end: row_start.saturating_add(row_count),
        }),
    );
    out.line_count()
        .saturating_sub(before)
        .min(u16::MAX as usize) as u16
}

pub fn measure_markdown_inner_with_options(
    content: &str,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
) -> usize {
    let block = parse_markdown_cached(content, inline_options);
    measure_markdown_block(block.as_doc(), width, indent, dim, bctx, inline_options)
}

#[allow(clippy::too_many_arguments)]
pub fn render_retained_markdown_inner_range_with_options(
    out: &mut LineBuilder,
    content: &TranscriptContent,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
    row_start: usize,
    row_count: usize,
) -> u16 {
    if row_count == 0 {
        return 0;
    }
    let _perf = smelt_perf::perf::begin("render:markdown:retained_range");
    let before = out.line_count();
    let parsed = parse_retained_markdown_cached(content, inline_options);
    let read = content.read();
    let source = MarkdownSource::Retained(&read);
    let clip = Some(RowClip {
        start: row_start,
        end: row_start.saturating_add(row_count),
    });
    if indent.is_empty() && bctx.is_none() {
        let mut parsed = parsed.borrow_mut();
        let layout_index =
            ensure_retained_markdown_layout(&mut parsed, source, width, dim, inline_options);
        let layout = &parsed.layouts[layout_index];
        let first_after = layout
            .completed_states
            .partition_point(|state| state.rows <= row_start);
        let first_doc = first_after.saturating_sub(1).min(parsed.completed.len());
        let mut state = layout
            .completed_states
            .get(first_doc)
            .copied()
            .unwrap_or_default();
        render_markdown_docs_from_state(
            out,
            parsed.docs(source, inline_options).skip(first_doc),
            width,
            indent,
            dim,
            bctx,
            inline_options,
            clip,
            &mut state,
        );
    } else {
        let parsed = parsed.borrow();
        render_markdown_docs(
            out,
            parsed.docs(source, inline_options),
            width,
            indent,
            dim,
            bctx,
            inline_options,
            clip,
        );
    }
    refresh_retained_markdown_cache_weight(&parsed);
    out.line_count()
        .saturating_sub(before)
        .min(u16::MAX as usize) as u16
}

pub fn measure_retained_markdown_inner_with_options(
    content: &TranscriptContent,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
) -> usize {
    let parsed = parse_retained_markdown_cached(content, inline_options);
    let read = content.read();
    let source = MarkdownSource::Retained(&read);
    let rows = if indent.is_empty() && bctx.is_none() {
        let mut parsed = parsed.borrow_mut();
        let layout_index =
            ensure_retained_markdown_layout(&mut parsed, source, width, dim, inline_options);
        parsed.layouts[layout_index].total_state.rows
    } else {
        let parsed = parsed.borrow();
        measure_markdown_docs(
            parsed.docs(source, inline_options),
            width,
            indent,
            dim,
            bctx,
            inline_options,
        )
    };
    refresh_retained_markdown_cache_weight(&parsed);
    rows
}

struct RetainedMarkdownEdge {
    nodes: Vec<Arc<[MarkdownNode]>>,
    row_start: usize,
    row_count: usize,
    truncated: bool,
}

impl RetainedMarkdownEdge {
    fn window(&self) -> ContentTextWindow {
        ContentTextWindow {
            row_count: self.row_count,
            truncated: self.truncated,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn retained_markdown_edge(
    read: &ContentRead<'_>,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
    max_rows: usize,
    tail: bool,
) -> RetainedMarkdownEdge {
    if max_rows == 0 {
        return RetainedMarkdownEdge {
            nodes: Vec::new(),
            row_start: 0,
            row_count: 0,
            truncated: !read.is_empty(),
        };
    }

    let source = MarkdownSource::Retained(read);
    let range_count = read.markdown_range_count();
    let mut nodes = Vec::new();
    let mut state = FlowState::default();

    if tail {
        let target_rows = max_rows.saturating_add(2);
        let mut start = range_count;
        let mut estimated_rows = 0usize;
        while start > 0 && estimated_rows <= target_rows {
            start -= 1;
            let range = read
                .markdown_range(start)
                .expect("retained Markdown range index");
            let parsed = parse_retained_markdown_nodes(
                read.slice(range.clone()).into_owned(),
                range.start,
                inline_options,
            );
            let mut doc_state = FlowState::default();
            measure_markdown_doc(
                MarkdownDoc {
                    source,
                    nodes: parsed.as_ref(),
                },
                width,
                indent,
                dim,
                bctx,
                inline_options,
                &mut doc_state,
            );
            estimated_rows = estimated_rows.saturating_add(doc_state.rows);
            nodes.push(parsed);
        }
        nodes.reverse();
        for parsed in &nodes {
            measure_markdown_doc(
                MarkdownDoc {
                    source,
                    nodes: parsed.as_ref(),
                },
                width,
                indent,
                dim,
                bctx,
                inline_options,
                &mut state,
            );
        }
        let row_count = state.rows.min(max_rows);
        RetainedMarkdownEdge {
            nodes,
            row_start: state.rows.saturating_sub(row_count),
            row_count,
            truncated: start > 0 || state.rows > max_rows,
        }
    } else {
        let mut next = 0usize;
        while next < range_count && state.rows <= max_rows {
            let range = read
                .markdown_range(next)
                .expect("retained Markdown range index");
            let parsed = parse_retained_markdown_nodes(
                read.slice(range.clone()).into_owned(),
                range.start,
                inline_options,
            );
            measure_markdown_doc(
                MarkdownDoc {
                    source,
                    nodes: parsed.as_ref(),
                },
                width,
                indent,
                dim,
                bctx,
                inline_options,
                &mut state,
            );
            nodes.push(parsed);
            next = next.saturating_add(1);
        }
        RetainedMarkdownEdge {
            nodes,
            row_start: 0,
            row_count: state.rows.min(max_rows),
            truncated: next < range_count || state.rows > max_rows,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn measure_retained_markdown_inner_edge_with_options(
    content: &TranscriptContent,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
    max_rows: usize,
) -> ContentTextWindow {
    retained_markdown_edge(
        &content.read(),
        width,
        indent,
        dim,
        bctx,
        inline_options,
        max_rows,
        false,
    )
    .window()
}

#[allow(clippy::too_many_arguments)]
pub fn render_retained_markdown_inner_edge_with_options(
    out: &mut LineBuilder,
    content: &TranscriptContent,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
    max_rows: usize,
    tail: bool,
) -> ContentTextWindow {
    let read = content.read();
    let edge = retained_markdown_edge(
        &read,
        width,
        indent,
        dim,
        bctx,
        inline_options,
        max_rows,
        tail,
    );
    let source = MarkdownSource::Retained(&read);
    render_markdown_docs(
        out,
        edge.nodes.iter().map(|nodes| MarkdownDoc {
            source,
            nodes: nodes.as_ref(),
        }),
        width,
        indent,
        dim,
        bctx,
        inline_options,
        Some(RowClip {
            start: edge.row_start,
            end: edge.row_start.saturating_add(edge.row_count),
        }),
    );
    edge.window()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn markdown_range_has_visible_text_with_options(
    content: &str,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
    row_start: usize,
    row_count: usize,
) -> bool {
    if row_count == 0 {
        return false;
    }
    let block = parse_markdown_cached(content, inline_options);
    markdown_block_range_has_visible_text(
        block.as_doc(),
        width,
        indent,
        dim,
        bctx,
        inline_options,
        RowClip {
            start: row_start,
            end: row_start.saturating_add(row_count),
        },
    )
}

const MARKDOWN_PARSE_CACHE_CAP: usize = 128;
const MARKDOWN_PARSE_CACHE_BYTES: usize = 16 * 1024 * 1024;

thread_local! {
    static MARKDOWN_PARSE_CACHE: RefCell<MarkdownParseCache> = const { RefCell::new(MarkdownParseCache::new()) };
    static RETAINED_MARKDOWN_PARSE_CACHE: RefCell<RetainedMarkdownParseCache> = const { RefCell::new(RetainedMarkdownParseCache::new()) };
}

#[derive(Clone)]
struct ParsedMarkdown {
    source: Arc<str>,
    nodes: Arc<[MarkdownNode]>,
}

impl ParsedMarkdown {
    fn as_doc(&self) -> MarkdownDoc<'_> {
        MarkdownDoc {
            source: MarkdownSource::Inline(self.source.as_ref()),
            nodes: self.nodes.as_ref(),
        }
    }

    fn dynamic_retained_bytes(&self) -> usize {
        self.source
            .len()
            .saturating_add(arc_header_bytes())
            .saturating_add(markdown_arc_retained_bytes(&self.nodes))
    }
}

#[derive(Clone, Copy)]
enum MarkdownSource<'a> {
    Inline(&'a str),
    Retained(&'a ContentRead<'a>),
}

impl<'a> MarkdownSource<'a> {
    fn slice(self, range: std::ops::Range<usize>) -> Cow<'a, str> {
        match self {
            Self::Inline(source) => Cow::Borrowed(smelt_buffer::text::slice(source, range)),
            Self::Retained(source) => source.slice(range),
        }
    }
}

#[derive(Clone, Copy)]
struct MarkdownDoc<'a> {
    source: MarkdownSource<'a>,
    nodes: &'a [MarkdownNode],
}

struct RetainedMarkdownParseCacheEntry {
    content_id: ContentId,
    inline_options: InlineOptions,
    parsed: Rc<RefCell<RetainedParsedMarkdown>>,
    retained_bytes: usize,
}

struct RetainedMarkdownDoc {
    range: Range<usize>,
    nodes: OnceCell<Arc<[MarkdownNode]>>,
}

impl RetainedMarkdownDoc {
    fn new(range: Range<usize>) -> Self {
        Self {
            range,
            nodes: OnceCell::new(),
        }
    }

    fn nodes(&self, source: MarkdownSource<'_>, inline_options: &InlineOptions) -> &[MarkdownNode] {
        self.nodes
            .get_or_init(|| {
                let start = self.range.start;
                parse_retained_markdown_nodes(
                    source.slice(self.range.clone()).into_owned(),
                    start,
                    inline_options,
                )
            })
            .as_ref()
    }
}

struct RetainedParsedMarkdown {
    revision: Option<u64>,
    content_len: usize,
    completed_end: usize,
    completed: Vec<Rc<RetainedMarkdownDoc>>,
    suffix: Arc<[MarkdownNode]>,
    layout_clock: u64,
    layouts: Vec<RetainedMarkdownLayout>,
}

struct RetainedMarkdownLayout {
    width: usize,
    completed_states: Vec<FlowState>,
    suffix_revision: Option<u64>,
    total_state: FlowState,
    last_used: u64,
}

impl RetainedParsedMarkdown {
    fn empty() -> Self {
        Self {
            revision: None,
            content_len: 0,
            completed_end: 0,
            completed: Vec::new(),
            suffix: Arc::from([]),
            layout_clock: 0,
            layouts: Vec::new(),
        }
    }

    fn docs<'a>(
        &'a self,
        source: MarkdownSource<'a>,
        inline_options: &'a InlineOptions,
    ) -> impl Iterator<Item = MarkdownDoc<'a>> + 'a {
        self.completed
            .iter()
            .map(move |doc| MarkdownDoc {
                source,
                nodes: doc.nodes(source, inline_options),
            })
            .chain(std::iter::once(MarkdownDoc {
                source,
                nodes: self.suffix.as_ref(),
            }))
    }

    fn dynamic_retained_bytes(&self) -> usize {
        self.completed
            .capacity()
            .saturating_mul(std::mem::size_of::<Rc<RetainedMarkdownDoc>>())
            .saturating_add(
                self.completed
                    .iter()
                    .map(|doc| {
                        rc_header_bytes()
                            .saturating_add(std::mem::size_of::<RetainedMarkdownDoc>())
                            .saturating_add(
                                doc.nodes
                                    .get()
                                    .map_or(0, |nodes| markdown_arc_retained_bytes(nodes)),
                            )
                    })
                    .sum::<usize>(),
            )
            .saturating_add(markdown_arc_retained_bytes(&self.suffix))
            .saturating_add(
                self.layouts
                    .capacity()
                    .saturating_mul(std::mem::size_of::<RetainedMarkdownLayout>()),
            )
            .saturating_add(
                self.layouts
                    .iter()
                    .map(|layout| {
                        layout
                            .completed_states
                            .capacity()
                            .saturating_mul(std::mem::size_of::<FlowState>())
                    })
                    .sum::<usize>(),
            )
    }

    fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.dynamic_retained_bytes())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct MarkdownParseCacheKey {
    content_len: usize,
    content_hash: u64,
    file_icon_options_hash: u64,
}

struct MarkdownParseCacheEntry {
    key: MarkdownParseCacheKey,
    inline_options: InlineOptions,
    parsed: ParsedMarkdown,
    retained_bytes: usize,
}

struct MarkdownParseCache {
    entries: VecDeque<MarkdownParseCacheEntry>,
    retained_bytes: usize,
}

impl MarkdownParseCache {
    const fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            retained_bytes: 0,
        }
    }

    fn push_front(&mut self, entry: MarkdownParseCacheEntry) {
        self.retained_bytes = self.retained_bytes.saturating_add(entry.retained_bytes);
        self.entries.push_front(entry);
        self.evict_to_budget();
    }

    fn evict_to_budget(&mut self) {
        while self.entries.len() > MARKDOWN_PARSE_CACHE_CAP
            || self.measured_retained_bytes() > MARKDOWN_PARSE_CACHE_BYTES
        {
            let Some(entry) = self.entries.pop_back() else {
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(entry.retained_bytes);
        }
    }

    fn measured_retained_bytes(&self) -> usize {
        self.retained_bytes.saturating_add(
            self.entries
                .capacity()
                .saturating_mul(std::mem::size_of::<MarkdownParseCacheEntry>()),
        )
    }

    #[cfg(test)]
    fn clear(&mut self) {
        self.entries = VecDeque::new();
        self.retained_bytes = 0;
    }
}

struct RetainedMarkdownParseCache {
    entries: VecDeque<RetainedMarkdownParseCacheEntry>,
    retained_bytes: usize,
}

impl RetainedMarkdownParseCache {
    const fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            retained_bytes: 0,
        }
    }

    fn push_front(&mut self, entry: RetainedMarkdownParseCacheEntry) {
        self.retained_bytes = self.retained_bytes.saturating_add(entry.retained_bytes);
        self.entries.push_front(entry);
    }

    fn refresh_entry(&mut self, parsed: &Rc<RefCell<RetainedParsedMarkdown>>) {
        let parsed_bytes = retained_parsed_allocation_bytes(parsed);
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| Rc::ptr_eq(&entry.parsed, parsed))
        else {
            return;
        };
        let retained_bytes = entry
            .inline_options
            .dynamic_retained_bytes()
            .saturating_add(parsed_bytes);
        self.retained_bytes = self
            .retained_bytes
            .saturating_sub(entry.retained_bytes)
            .saturating_add(retained_bytes);
        entry.retained_bytes = retained_bytes;
        self.evict_to_budget();
    }

    fn evict_to_budget(&mut self) {
        while self.entries.len() > MARKDOWN_PARSE_CACHE_CAP
            || self.measured_retained_bytes() > MARKDOWN_PARSE_CACHE_BYTES
        {
            let Some(entry) = self.entries.pop_back() else {
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(entry.retained_bytes);
        }
    }

    fn measured_retained_bytes(&self) -> usize {
        self.retained_bytes.saturating_add(
            self.entries
                .capacity()
                .saturating_mul(std::mem::size_of::<RetainedMarkdownParseCacheEntry>()),
        )
    }

    #[cfg(test)]
    fn clear(&mut self) {
        self.entries = VecDeque::new();
        self.retained_bytes = 0;
    }
}

const fn arc_header_bytes() -> usize {
    2 * std::mem::size_of::<usize>()
}

const fn rc_header_bytes() -> usize {
    2 * std::mem::size_of::<usize>()
}

fn markdown_arc_retained_bytes(nodes: &[MarkdownNode]) -> usize {
    arc_header_bytes().saturating_add(markdown_nodes_retained_bytes(nodes))
}

fn retained_parsed_allocation_bytes(parsed: &Rc<RefCell<RetainedParsedMarkdown>>) -> usize {
    let parsed = parsed.borrow();
    rc_header_bytes()
        .saturating_add(
            std::mem::size_of::<RefCell<RetainedParsedMarkdown>>()
                .saturating_sub(std::mem::size_of::<RetainedParsedMarkdown>()),
        )
        .saturating_add(parsed.retained_bytes())
}

fn parse_markdown_cached(content: &str, inline_options: &InlineOptions) -> ParsedMarkdown {
    let key = markdown_parse_cache_key(content, inline_options);
    if let Some(parsed) = MARKDOWN_PARSE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let pos = cache.entries.iter().position(|entry| {
            entry.key == key
                && entry.inline_options == *inline_options
                && entry.parsed.source.as_ref() == content
        })?;
        let entry = cache.entries.remove(pos)?;
        let parsed = entry.parsed.clone();
        cache.entries.push_front(entry);
        Some(parsed)
    }) {
        return parsed;
    }

    let block = parse_markdown_with_options(content, inline_options);
    let parsed = ParsedMarkdown {
        source: Arc::from(content),
        nodes: Arc::from(block.nodes.into_boxed_slice()),
    };
    let retained_bytes = inline_options
        .dynamic_retained_bytes()
        .saturating_add(parsed.dynamic_retained_bytes());
    MARKDOWN_PARSE_CACHE.with(|cache| {
        cache.borrow_mut().push_front(MarkdownParseCacheEntry {
            key,
            inline_options: inline_options.clone(),
            parsed: parsed.clone(),
            retained_bytes,
        });
    });
    parsed
}

fn parse_retained_markdown_cached(
    content: &TranscriptContent,
    inline_options: &InlineOptions,
) -> Rc<RefCell<RetainedParsedMarkdown>> {
    let parsed = RETAINED_MARKDOWN_PARSE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(pos) = cache.entries.iter().position(|entry| {
            entry.content_id == content.id() && entry.inline_options == *inline_options
        }) {
            let entry = cache
                .entries
                .remove(pos)
                .expect("retained Markdown cache position exists");
            let parsed = Rc::clone(&entry.parsed);
            cache.entries.push_front(entry);
            parsed
        } else {
            let parsed = Rc::new(RefCell::new(RetainedParsedMarkdown::empty()));
            let retained_bytes = inline_options
                .dynamic_retained_bytes()
                .saturating_add(retained_parsed_allocation_bytes(&parsed));
            cache.push_front(RetainedMarkdownParseCacheEntry {
                content_id: content.id(),
                inline_options: inline_options.clone(),
                parsed: Rc::clone(&parsed),
                retained_bytes,
            });
            parsed
        }
    });

    update_retained_markdown(&parsed, content, inline_options);
    refresh_retained_markdown_cache_weight(&parsed);
    parsed
}

fn refresh_retained_markdown_cache_weight(parsed: &Rc<RefCell<RetainedParsedMarkdown>>) {
    RETAINED_MARKDOWN_PARSE_CACHE.with(|cache| cache.borrow_mut().refresh_entry(parsed));
}

fn update_retained_markdown(
    parsed: &Rc<RefCell<RetainedParsedMarkdown>>,
    content: &TranscriptContent,
    inline_options: &InlineOptions,
) {
    let (cached_revision, cached_len, completed_end) = {
        let parsed = parsed.borrow();
        (parsed.revision, parsed.content_len, parsed.completed_end)
    };
    let read = content.read();
    if cached_revision == Some(read.revision()) {
        return;
    }

    let suffix_range = read.markdown_suffix_range();
    let reset = read.len() < cached_len || suffix_range.start < completed_end;
    let completed_start = if reset { 0 } else { completed_end };
    let completed_docs = read
        .markdown_completed_ranges_after(completed_start)
        .into_iter()
        .map(|range| Rc::new(RetainedMarkdownDoc::new(range)))
        .collect::<Vec<_>>();
    let suffix_start = suffix_range.start;
    let suffix_source = read.slice(suffix_range).into_owned();
    let revision = read.revision();
    let content_len = read.len();
    drop(read);

    let suffix = parse_retained_markdown_nodes(suffix_source, suffix_start, inline_options);

    let mut parsed = parsed.borrow_mut();
    if reset {
        parsed.completed.clear();
        parsed.layouts.clear();
    }
    parsed.completed.extend(completed_docs);
    parsed.revision = Some(revision);
    parsed.content_len = content_len;
    parsed.completed_end = suffix_start;
    parsed.suffix = suffix;
}

fn parse_retained_markdown_nodes(
    source: String,
    source_start: usize,
    inline_options: &InlineOptions,
) -> Arc<[MarkdownNode]> {
    let mut nodes = parse_markdown_with_options(&source, inline_options).nodes;
    shift_markdown_nodes(&mut nodes, source_start);
    Arc::from(nodes.into_boxed_slice())
}

fn shift_markdown_nodes(nodes: &mut [MarkdownNode], offset: usize) {
    let shift = |range: &mut std::ops::Range<usize>| {
        range.start = range.start.saturating_add(offset);
        range.end = range.end.saturating_add(offset);
    };
    for node in nodes {
        match node {
            MarkdownNode::Source { range } | MarkdownNode::Rule { range } => shift(range),
            MarkdownNode::Text { range, lines, .. } => {
                shift(range);
                for line in lines {
                    shift(&mut line.source);
                }
            }
            MarkdownNode::Code { range, body, .. } => {
                shift(range);
                for line in body {
                    shift(line);
                }
            }
            MarkdownNode::Table { range, .. } => shift(range),
        }
    }
}

const RETAINED_MARKDOWN_LAYOUT_WIDTHS: usize = 2;

fn ensure_retained_markdown_layout(
    parsed: &mut RetainedParsedMarkdown,
    source: MarkdownSource<'_>,
    width: usize,
    dim: bool,
    inline_options: &InlineOptions,
) -> usize {
    parsed.layout_clock = parsed
        .layout_clock
        .checked_add(1)
        .expect("retained Markdown layout clock overflow");
    let mut layout = parsed
        .layouts
        .iter()
        .position(|layout| layout.width == width)
        .map(|index| parsed.layouts.swap_remove(index))
        .unwrap_or_else(|| RetainedMarkdownLayout {
            width,
            completed_states: vec![FlowState::default()],
            suffix_revision: None,
            total_state: FlowState::default(),
            last_used: 0,
        });

    let measured_docs = layout.completed_states.len().saturating_sub(1);
    let mut state = layout.completed_states.last().copied().unwrap_or_default();
    for doc in parsed.completed.iter().skip(measured_docs) {
        measure_markdown_doc(
            MarkdownDoc {
                source,
                nodes: doc.nodes(source, inline_options),
            },
            width,
            "",
            dim,
            None,
            inline_options,
            &mut state,
        );
        layout.completed_states.push(state);
    }

    if layout.suffix_revision != parsed.revision {
        state = layout.completed_states.last().copied().unwrap_or_default();
        measure_markdown_doc(
            MarkdownDoc {
                source,
                nodes: parsed.suffix.as_ref(),
            },
            width,
            "",
            dim,
            None,
            inline_options,
            &mut state,
        );
        layout.total_state = state;
        layout.suffix_revision = parsed.revision;
    }
    layout.last_used = parsed.layout_clock;

    if parsed.layouts.len() >= RETAINED_MARKDOWN_LAYOUT_WIDTHS {
        let oldest = parsed
            .layouts
            .iter()
            .enumerate()
            .min_by_key(|(_, layout)| layout.last_used)
            .map(|(index, _)| index)
            .unwrap_or_default();
        parsed.layouts.swap_remove(oldest);
    }
    parsed.layouts.push(layout);
    parsed.layouts.len().saturating_sub(1)
}

fn markdown_parse_cache_key(
    content: &str,
    inline_options: &InlineOptions,
) -> MarkdownParseCacheKey {
    MarkdownParseCacheKey {
        content_len: content.len(),
        content_hash: hash_value(content),
        file_icon_options_hash: hash_value(&inline_options.file_icons),
    }
}

fn hash_value<T: Hash + ?Sized>(value: &T) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[derive(Clone, Copy)]
struct RowClip {
    start: usize,
    end: usize,
}

impl RowClip {
    fn contains(self, row: usize) -> bool {
        row >= self.start && row < self.end
    }

    fn intersects(self, start: usize, rows: usize) -> bool {
        let end = start.saturating_add(rows);
        start < self.end && end > self.start
    }
}

fn should_emit(clip: Option<RowClip>, row: usize) -> bool {
    clip.is_none_or(|clip| clip.contains(row))
}

#[allow(clippy::too_many_arguments)]
fn render_markdown_block(
    out: &mut LineBuilder,
    block: MarkdownDoc<'_>,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
    clip: Option<RowClip>,
) -> u16 {
    render_markdown_docs(
        out,
        std::iter::once(block),
        width,
        indent,
        dim,
        bctx,
        inline_options,
        clip,
    )
    .min(u16::MAX as usize) as u16
}

#[allow(clippy::too_many_arguments)]
fn render_markdown_docs<'a>(
    out: &mut LineBuilder,
    blocks: impl IntoIterator<Item = MarkdownDoc<'a>>,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
    clip: Option<RowClip>,
) -> usize {
    let mut state = RenderState::default();
    render_markdown_docs_from_state(
        out,
        blocks,
        width,
        indent,
        dim,
        bctx,
        inline_options,
        clip,
        &mut state,
    );
    state.rows
}

#[allow(clippy::too_many_arguments)]
fn render_markdown_docs_from_state<'a>(
    out: &mut LineBuilder,
    blocks: impl IntoIterator<Item = MarkdownDoc<'a>>,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
    clip: Option<RowClip>,
    state: &mut RenderState,
) {
    let max_cols = if let Some(b) = bctx {
        b.inner_w
    } else {
        width.saturating_sub(display_width(indent) + 1)
    };
    let ctx = RenderTextCtx {
        max_cols,
        indent,
        dim,
        bctx,
        inline_options,
        clip,
    };

    for block in blocks {
        for node in block.nodes {
            if clip.is_some_and(|clip| state.rows >= clip.end) {
                break;
            }
            match node {
                MarkdownNode::Source { range } => {
                    let source = block.source.slice(range.clone());
                    render_source_lines(out, &source, MarkdownTextKind::Paragraph, &ctx, state);
                }
                MarkdownNode::Text { lines, kind, .. } => {
                    render_text_lines(out, block.source, lines, *kind, &ctx, state);
                }
                MarkdownNode::Code { lang, body, .. } => {
                    render_block_gap(out, state, clip);
                    let code_source = markdown_code_source(block.source, body);
                    let code_lines: Vec<&str> = code_source.lines().collect();
                    let code_block = parse_code_block(&code_lines, lang);
                    if let Some(clip) = clip {
                        let rows = measure_code_block(&code_block, width);
                        if clip.intersects(state.rows, rows) {
                            let inherited_style = out.current_style();
                            let mut buf = smelt_core::buffer::Buffer::new(
                                smelt_core::buffer::BufId(0),
                                smelt_core::buffer::BufCreateOpts::default(),
                            );
                            let outcome = {
                                let mut col =
                                    LineBuilder::new(&mut buf, out.theme(), width.max(1) as u16);
                                col.push(None, inherited_style);
                                render_code_block(&mut col, &code_block, width, dim, bctx, true);
                                col.finish()
                            };
                            if outcome.was_wrapped {
                                out.mark_wrapped();
                            }
                            emit_temp_rows(out, &buf, width, state.rows, rows, clip, None);
                        }
                        state.rows = state.rows.saturating_add(rows);
                    } else {
                        state.rows = state.rows.saturating_add(usize::from(render_code_block(
                            out,
                            &code_block,
                            width,
                            dim,
                            bctx,
                            true,
                        )));
                    }
                    state.last_content_was_heading = false;
                    state.prev_was_block = true;
                }
                MarkdownNode::Table {
                    range,
                    alignments,
                    rows,
                } => {
                    render_block_gap(out, state, clip);
                    if let Some(clip) = clip {
                        let table_rows = usize::from(measure_markdown_table_with_options(
                            rows,
                            alignments,
                            width,
                            dim,
                            bctx,
                            indent,
                            inline_options,
                        ));
                        if clip.intersects(state.rows, table_rows) {
                            let inherited_style = out.current_style();
                            let mut buf = smelt_core::buffer::Buffer::new(
                                smelt_core::buffer::BufId(0),
                                smelt_core::buffer::BufCreateOpts::default(),
                            );
                            let outcome = {
                                let mut col =
                                    LineBuilder::new(&mut buf, out.theme(), width.max(1) as u16);
                                col.push(None, inherited_style);
                                let start = col.line_count();
                                render_markdown_table_with_options(
                                    &mut col,
                                    rows,
                                    alignments,
                                    width,
                                    dim,
                                    bctx,
                                    indent,
                                    inline_options,
                                );
                                let source = block.source.slice(range.clone());
                                let source = source.trim_end_matches(['\r', '\n']);
                                col.stamp_copy_group(start, source);
                                col.finish()
                            };
                            if outcome.was_wrapped {
                                out.mark_wrapped();
                            }
                            emit_temp_rows(out, &buf, width, state.rows, table_rows, clip, None);
                        }
                        state.rows = state.rows.saturating_add(table_rows);
                    } else {
                        let start = out.line_count();
                        state.rows = state.rows.saturating_add(usize::from(
                            render_markdown_table_with_options(
                                out,
                                rows,
                                alignments,
                                width,
                                dim,
                                bctx,
                                indent,
                                inline_options,
                            ),
                        ));
                        let source = block.source.slice(range.clone());
                        let source = source.trim_end_matches(['\r', '\n']);
                        out.stamp_copy_group(start, source);
                    }
                    state.last_content_was_heading = false;
                    state.prev_was_block = true;
                }
                MarkdownNode::Rule { .. } => {
                    render_block_gap(out, state, clip);
                    if should_emit(clip, state.rows) {
                        render_horizontal_rule(out, bctx, indent);
                    }
                    state.rows = state.rows.saturating_add(1);
                    state.last_content_was_heading = false;
                    state.prev_was_block = true;
                }
            }
        }
    }
}

fn emit_temp_rows(
    out: &mut LineBuilder,
    buf: &smelt_core::buffer::Buffer,
    width: usize,
    base_row: usize,
    rows: usize,
    clip: RowClip,
    style_overlay: Option<(bool, bool)>,
) {
    let start = clip.start.saturating_sub(base_row).min(rows);
    let end = clip.end.saturating_sub(base_row).min(rows);
    for row in start..end {
        let Ok(buffer_row) = u16::try_from(row) else {
            break;
        };
        apply_temp_decoration(out, buf, row, true);
        emit_buffer_row_clipped(
            buf,
            buffer_row,
            width.min(u16::MAX as usize) as u16,
            out,
            style_overlay,
        );
        out.newline();
    }
}

#[allow(clippy::too_many_arguments)]
fn markdown_block_range_has_visible_text(
    block: MarkdownDoc<'_>,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
    clip: RowClip,
) -> bool {
    let max_cols = if let Some(b) = bctx {
        b.inner_w
    } else {
        width.saturating_sub(display_width(indent) + 1)
    };
    let mut state = FlowState::default();

    for node in block.nodes {
        if state.rows >= clip.end {
            break;
        }
        match node {
            MarkdownNode::Source { range } => {
                let source = block.source.slice(range.clone());
                if source_lines_range_has_visible_text(
                    &source,
                    MarkdownTextKind::Paragraph,
                    max_cols,
                    dim,
                    inline_options,
                    clip,
                    &mut state,
                ) {
                    return true;
                }
            }
            MarkdownNode::Text { lines, kind, .. } => {
                if text_lines_range_has_visible_text(
                    block.source,
                    lines,
                    *kind,
                    max_cols,
                    dim,
                    clip,
                    &mut state,
                ) {
                    return true;
                }
            }
            MarkdownNode::Code { lang, body, .. } => {
                measure_block_gap(&mut state);
                let code_source = markdown_code_source(block.source, body);
                let code_lines: Vec<&str> = code_source.lines().collect();
                let code_block = parse_code_block(&code_lines, lang);
                let rows = measure_code_block(&code_block, width);
                if clip.intersects(state.rows, rows)
                    && code_lines.iter().any(|line| !line.trim().is_empty())
                {
                    return true;
                }
                state.rows = state.rows.saturating_add(rows);
                state.last_content_was_heading = false;
                state.prev_was_block = true;
            }
            MarkdownNode::Table {
                alignments, rows, ..
            } => {
                measure_block_gap(&mut state);
                let table_rows = usize::from(measure_markdown_table_with_options(
                    rows,
                    alignments,
                    width,
                    dim,
                    bctx,
                    indent,
                    inline_options,
                ));
                if clip.intersects(state.rows, table_rows)
                    && rows.iter().flatten().any(|cell| !cell.trim().is_empty())
                {
                    return true;
                }
                state.rows = state.rows.saturating_add(table_rows);
                state.last_content_was_heading = false;
                state.prev_was_block = true;
            }
            MarkdownNode::Rule { .. } => {
                measure_block_gap(&mut state);
                if clip.contains(state.rows) {
                    return true;
                }
                state.rows = state.rows.saturating_add(1);
                state.last_content_was_heading = false;
                state.prev_was_block = true;
            }
        }
    }

    false
}

fn measure_markdown_block(
    block: MarkdownDoc<'_>,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
) -> usize {
    measure_markdown_docs(
        std::iter::once(block),
        width,
        indent,
        dim,
        bctx,
        inline_options,
    )
}

fn measure_markdown_docs<'a>(
    blocks: impl IntoIterator<Item = MarkdownDoc<'a>>,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
) -> usize {
    let mut state = MeasureState::default();
    for block in blocks {
        measure_markdown_doc(block, width, indent, dim, bctx, inline_options, &mut state);
    }
    state.rows
}

#[allow(clippy::too_many_arguments)]
fn measure_markdown_doc(
    block: MarkdownDoc<'_>,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
    state: &mut MeasureState,
) {
    let max_cols = if let Some(b) = bctx {
        b.inner_w
    } else {
        width.saturating_sub(display_width(indent) + 1)
    };
    for node in block.nodes {
        match node {
            MarkdownNode::Source { range } => {
                let source = block.source.slice(range.clone());
                measure_source_lines(
                    &source,
                    MarkdownTextKind::Paragraph,
                    max_cols,
                    dim,
                    inline_options,
                    state,
                );
            }
            MarkdownNode::Text { lines, kind, .. } => {
                measure_text_lines(block.source, lines, *kind, max_cols, dim, state);
            }
            MarkdownNode::Code { lang, body, .. } => {
                measure_block_gap(state);
                let code_source = markdown_code_source(block.source, body);
                let code_lines: Vec<&str> = code_source.lines().collect();
                let code_block = parse_code_block(&code_lines, lang);
                state.rows = state
                    .rows
                    .saturating_add(measure_code_block(&code_block, width));
                state.last_content_was_heading = false;
                state.prev_was_block = true;
            }
            MarkdownNode::Table {
                alignments, rows, ..
            } => {
                measure_block_gap(state);
                state.rows =
                    state
                        .rows
                        .saturating_add(usize::from(measure_markdown_table_with_options(
                            rows,
                            alignments,
                            width,
                            dim,
                            bctx,
                            indent,
                            inline_options,
                        )));
                state.last_content_was_heading = false;
                state.prev_was_block = true;
            }
            MarkdownNode::Rule { .. } => {
                measure_block_gap(state);
                state.rows = state.rows.saturating_add(1);
                state.last_content_was_heading = false;
                state.prev_was_block = true;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FlowState {
    rows: usize,
    last_content_was_heading: bool,
    pending_blank: bool,
    prev_was_block: bool,
}

type MeasureState = FlowState;
type RenderState = FlowState;

fn measure_block_gap(state: &mut MeasureState) {
    let mut gap_emitted = false;
    if state.pending_blank {
        state.rows = state.rows.saturating_add(1);
        state.pending_blank = false;
        gap_emitted = true;
    }
    if state.rows > 0 && !gap_emitted && !state.last_content_was_heading {
        state.rows = state.rows.saturating_add(1);
    }
}

fn measure_text_gap(state: &mut MeasureState, kind: MarkdownTextKind) -> bool {
    if state.rows == 0 {
        state.pending_blank = false;
        return false;
    }
    if kind == MarkdownTextKind::List && !state.prev_was_block {
        state.pending_blank = false;
        return false;
    }
    let before = state.rows;
    measure_block_gap(state);
    state.rows != before
}

fn markdown_source_lines<'a>(
    source: MarkdownSource<'a>,
    lines: &[MarkdownLine],
) -> Vec<Cow<'a, str>> {
    lines
        .iter()
        .map(|line| source.slice(line.source.clone()))
        .collect()
}

fn markdown_code_source(source: MarkdownSource<'_>, body: &[std::ops::Range<usize>]) -> String {
    let mut code = String::with_capacity(body.iter().map(std::ops::Range::len).sum());
    for range in body {
        code.push_str(&source.slice(range.clone()));
    }
    code
}

fn measure_source_lines(
    source: &str,
    kind: MarkdownTextKind,
    max_cols: usize,
    dim: bool,
    inline_options: &InlineOptions,
    state: &mut MeasureState,
) {
    let lines: Vec<&str> = source.lines().collect();
    let mut sink = MeasureSourceSink {
        max_cols,
        dim,
        kind,
        inline_options,
    };
    walk_text_lines(
        lines.len(),
        |i| lines[i],
        |i| is_markdown_list_item(lines[i]),
        kind,
        state,
        &mut sink,
    );
}

fn measure_text_lines(
    source: MarkdownSource<'_>,
    lines: &[MarkdownLine],
    kind: MarkdownTextKind,
    max_cols: usize,
    dim: bool,
    state: &mut MeasureState,
) {
    let source_lines = markdown_source_lines(source, lines);
    let mut sink = MeasureIrSink {
        lines,
        max_cols,
        dim,
        kind,
    };
    walk_text_lines(
        lines.len(),
        |i| source_lines[i].as_ref(),
        |i| is_markdown_list_item(source_lines[i].as_ref()),
        kind,
        state,
        &mut sink,
    );
}

fn source_lines_range_has_visible_text(
    source: &str,
    kind: MarkdownTextKind,
    max_cols: usize,
    dim: bool,
    inline_options: &InlineOptions,
    clip: RowClip,
    state: &mut FlowState,
) -> bool {
    let lines: Vec<&str> = source.lines().collect();
    let mut sink = ProbeSourceSink {
        max_cols,
        dim,
        kind,
        inline_options,
        clip,
        found: false,
    };
    walk_text_lines(
        lines.len(),
        |i| lines[i],
        |i| is_markdown_list_item(lines[i]),
        kind,
        state,
        &mut sink,
    );
    sink.found
}

fn text_lines_range_has_visible_text(
    source: MarkdownSource<'_>,
    lines: &[MarkdownLine],
    kind: MarkdownTextKind,
    max_cols: usize,
    dim: bool,
    clip: RowClip,
    state: &mut FlowState,
) -> bool {
    let source_lines = markdown_source_lines(source, lines);
    let mut sink = ProbeIrSink {
        lines,
        max_cols,
        dim,
        kind,
        clip,
        found: false,
    };
    walk_text_lines(
        lines.len(),
        |i| source_lines[i].as_ref(),
        |i| is_markdown_list_item(source_lines[i].as_ref()),
        kind,
        state,
        &mut sink,
    );
    sink.found
}

fn render_block_gap(out: &mut LineBuilder, state: &mut RenderState, clip: Option<RowClip>) {
    let mut gap_emitted = false;
    if state.pending_blank {
        if should_emit(clip, state.rows) {
            out.newline();
        }
        state.rows = state.rows.saturating_add(1);
        state.pending_blank = false;
        gap_emitted = true;
    }
    if state.rows > 0 && !gap_emitted && !state.last_content_was_heading {
        if should_emit(clip, state.rows) {
            out.newline();
        }
        state.rows = state.rows.saturating_add(1);
    }
}

struct RenderTextCtx<'a> {
    max_cols: usize,
    indent: &'a str,
    dim: bool,
    bctx: Option<&'a smelt_core::content::BoxContext>,
    inline_options: &'a InlineOptions,
    clip: Option<RowClip>,
}

fn render_text_gap(
    out: &mut LineBuilder,
    state: &mut RenderState,
    kind: MarkdownTextKind,
    clip: Option<RowClip>,
) -> bool {
    if state.rows == 0 {
        state.pending_blank = false;
        return false;
    }
    if kind == MarkdownTextKind::List && !state.prev_was_block {
        state.pending_blank = false;
        return false;
    }
    let before = state.rows;
    render_block_gap(out, state, clip);
    state.rows != before
}

fn render_source_lines(
    out: &mut LineBuilder,
    source: &str,
    kind: MarkdownTextKind,
    ctx: &RenderTextCtx<'_>,
    state: &mut RenderState,
) {
    let lines: Vec<&str> = source.lines().collect();
    let mut sink = RenderSourceSink { out, ctx, kind };
    walk_text_lines(
        lines.len(),
        |i| lines[i],
        |i| is_markdown_list_item(lines[i]),
        kind,
        state,
        &mut sink,
    );
}

fn render_text_lines(
    out: &mut LineBuilder,
    source: MarkdownSource<'_>,
    lines: &[MarkdownLine],
    kind: MarkdownTextKind,
    ctx: &RenderTextCtx<'_>,
    state: &mut RenderState,
) {
    let source_lines = markdown_source_lines(source, lines);
    let mut sink = RenderIrSink {
        out,
        ctx,
        lines,
        kind,
    };
    walk_text_lines(
        lines.len(),
        |i| source_lines[i].as_ref(),
        |i| is_markdown_list_item(source_lines[i].as_ref()),
        kind,
        state,
        &mut sink,
    );
}

trait TextFlowSink {
    fn text_gap(&mut self, state: &mut FlowState, kind: MarkdownTextKind) -> bool;
    fn blank_line(&mut self, state: &mut FlowState);
    fn emit_line(&mut self, index: usize, line: &str, state: &mut FlowState);
    fn done(&self, _state: &FlowState) -> bool {
        false
    }
}

fn walk_text_lines<'a>(
    len: usize,
    mut line_at: impl FnMut(usize) -> &'a str,
    mut is_list_item_at: impl FnMut(usize) -> bool,
    kind: MarkdownTextKind,
    state: &mut FlowState,
    sink: &mut impl TextFlowSink,
) {
    let mut started = false;
    let mut i = 0;
    while i < len {
        if sink.done(state) {
            break;
        }
        let line = line_at(i);
        if line.trim().is_empty() {
            let mut next_i = i + 1;
            while next_i < len && line_at(next_i).trim().is_empty() {
                next_i += 1;
            }
            if state.rows > 0
                && !state.last_content_was_heading
                && next_i < len
                && !is_list_item_at(next_i)
            {
                state.pending_blank = true;
            }
            i = next_i;
            continue;
        }

        let mut gap_emitted = false;
        if !started {
            gap_emitted = sink.text_gap(state, kind);
            started = true;
        }
        if state.pending_blank {
            sink.blank_line(state);
            if sink.done(state) {
                break;
            }
            gap_emitted = true;
        }
        if state.prev_was_block && !gap_emitted {
            sink.blank_line(state);
            if sink.done(state) {
                break;
            }
        }
        sink.emit_line(i, line, state);
        state.last_content_was_heading = kind == MarkdownTextKind::Heading;
        state.prev_was_block = false;
        i += 1;
    }
}

struct MeasureSourceSink<'a> {
    max_cols: usize,
    dim: bool,
    kind: MarkdownTextKind,
    inline_options: &'a InlineOptions,
}

impl TextFlowSink for MeasureSourceSink<'_> {
    fn text_gap(&mut self, state: &mut FlowState, kind: MarkdownTextKind) -> bool {
        measure_text_gap(state, kind)
    }

    fn blank_line(&mut self, state: &mut FlowState) {
        state.rows = state.rows.saturating_add(1);
        state.pending_blank = false;
    }

    fn emit_line(&mut self, _index: usize, line: &str, state: &mut FlowState) {
        let spans = fallback_markdown_line_spans(line, self.kind, self.dim, self.inline_options);
        state.rows = state
            .rows
            .saturating_add(wrap_inline_spans(&spans, self.max_cols).len());
    }
}

struct MeasureIrSink<'a> {
    lines: &'a [MarkdownLine],
    max_cols: usize,
    dim: bool,
    kind: MarkdownTextKind,
}

impl TextFlowSink for MeasureIrSink<'_> {
    fn text_gap(&mut self, state: &mut FlowState, kind: MarkdownTextKind) -> bool {
        measure_text_gap(state, kind)
    }

    fn blank_line(&mut self, state: &mut FlowState) {
        state.rows = state.rows.saturating_add(1);
        state.pending_blank = false;
    }

    fn emit_line(&mut self, index: usize, line: &str, state: &mut FlowState) {
        let spans = markdown_line_spans(line, &self.lines[index].spans, self.kind, self.dim);
        state.rows = state
            .rows
            .saturating_add(wrap_inline_spans(&spans, self.max_cols).len());
    }
}

struct ProbeSourceSink<'a> {
    max_cols: usize,
    dim: bool,
    kind: MarkdownTextKind,
    inline_options: &'a InlineOptions,
    clip: RowClip,
    found: bool,
}

impl TextFlowSink for ProbeSourceSink<'_> {
    fn text_gap(&mut self, state: &mut FlowState, kind: MarkdownTextKind) -> bool {
        measure_text_gap(state, kind)
    }

    fn blank_line(&mut self, state: &mut FlowState) {
        state.rows = state.rows.saturating_add(1);
        state.pending_blank = false;
    }

    fn emit_line(&mut self, _index: usize, line: &str, state: &mut FlowState) {
        let spans = fallback_markdown_line_spans(line, self.kind, self.dim, self.inline_options);
        probe_markdown_line(&spans, self.max_cols, self.clip, state, &mut self.found);
    }

    fn done(&self, state: &FlowState) -> bool {
        self.found || state.rows >= self.clip.end
    }
}

struct ProbeIrSink<'a> {
    lines: &'a [MarkdownLine],
    max_cols: usize,
    dim: bool,
    kind: MarkdownTextKind,
    clip: RowClip,
    found: bool,
}

impl TextFlowSink for ProbeIrSink<'_> {
    fn text_gap(&mut self, state: &mut FlowState, kind: MarkdownTextKind) -> bool {
        measure_text_gap(state, kind)
    }

    fn blank_line(&mut self, state: &mut FlowState) {
        state.rows = state.rows.saturating_add(1);
        state.pending_blank = false;
    }

    fn emit_line(&mut self, index: usize, line: &str, state: &mut FlowState) {
        let spans = markdown_line_spans(line, &self.lines[index].spans, self.kind, self.dim);
        probe_markdown_line(&spans, self.max_cols, self.clip, state, &mut self.found);
    }

    fn done(&self, state: &FlowState) -> bool {
        self.found || state.rows >= self.clip.end
    }
}

fn probe_markdown_line(
    spans: &[InlineSpan],
    max_cols: usize,
    clip: RowClip,
    state: &mut FlowState,
    found: &mut bool,
) {
    for row_spans in wrap_inline_spans(spans, max_cols) {
        if clip.contains(state.rows) && inline_spans_have_visible_text(&row_spans) {
            *found = true;
        }
        state.rows = state.rows.saturating_add(1);
        if *found || state.rows >= clip.end {
            break;
        }
    }
}

fn inline_spans_have_visible_text(spans: &[InlineSpan]) -> bool {
    spans.iter().any(|span| !span.text.trim().is_empty())
}

struct RenderSourceSink<'a, 'b, 'c> {
    out: &'a mut LineBuilder<'b>,
    ctx: &'a RenderTextCtx<'c>,
    kind: MarkdownTextKind,
}

impl TextFlowSink for RenderSourceSink<'_, '_, '_> {
    fn text_gap(&mut self, state: &mut FlowState, kind: MarkdownTextKind) -> bool {
        render_text_gap(self.out, state, kind, self.ctx.clip)
    }

    fn blank_line(&mut self, state: &mut FlowState) {
        if should_emit(self.ctx.clip, state.rows) {
            self.out.newline();
        }
        state.rows = state.rows.saturating_add(1);
        state.pending_blank = false;
    }

    fn emit_line(&mut self, _index: usize, line: &str, state: &mut FlowState) {
        let spans =
            fallback_markdown_line_spans(line, self.kind, self.ctx.dim, self.ctx.inline_options);
        render_markdown_line(self.out, line, &spans, self.ctx, state);
    }

    fn done(&self, state: &FlowState) -> bool {
        self.ctx.clip.is_some_and(|clip| state.rows >= clip.end)
    }
}

struct RenderIrSink<'a, 'b, 'c, 'd> {
    out: &'a mut LineBuilder<'b>,
    ctx: &'a RenderTextCtx<'c>,
    lines: &'d [MarkdownLine],
    kind: MarkdownTextKind,
}

impl TextFlowSink for RenderIrSink<'_, '_, '_, '_> {
    fn text_gap(&mut self, state: &mut FlowState, kind: MarkdownTextKind) -> bool {
        render_text_gap(self.out, state, kind, self.ctx.clip)
    }

    fn blank_line(&mut self, state: &mut FlowState) {
        if should_emit(self.ctx.clip, state.rows) {
            self.out.newline();
        }
        state.rows = state.rows.saturating_add(1);
        state.pending_blank = false;
    }

    fn emit_line(&mut self, index: usize, line: &str, state: &mut FlowState) {
        let spans = markdown_line_spans(line, &self.lines[index].spans, self.kind, self.ctx.dim);
        render_markdown_line(self.out, line, &spans, self.ctx, state);
    }

    fn done(&self, state: &FlowState) -> bool {
        self.ctx.clip.is_some_and(|clip| state.rows >= clip.end)
    }
}

fn render_markdown_line(
    out: &mut LineBuilder,
    line: &str,
    spans: &[InlineSpan],
    ctx: &RenderTextCtx<'_>,
    state: &mut RenderState,
) {
    let wrapped = wrap_inline_spans(spans, ctx.max_cols);
    for segment in wrapped_segments(out, &wrapped) {
        if should_emit(ctx.clip, state.rows) {
            segment.emit_with_source(out, line, |out, row_spans, _| {
                if let Some(b) = ctx.bctx {
                    b.print_left(out);
                    emit_inline_spans(out, row_spans);
                    b.print_right(out, inline_spans_width(row_spans));
                } else {
                    out.print(ctx.indent);
                    emit_inline_spans(out, row_spans);
                }
            });
            out.newline();
        }
        state.rows = state.rows.saturating_add(1);
        if ctx.clip.is_some_and(|clip| state.rows >= clip.end) {
            break;
        }
    }
}

fn fallback_markdown_line_spans(
    line: &str,
    kind: MarkdownTextKind,
    dim: bool,
    inline_options: &InlineOptions,
) -> Vec<InlineSpan> {
    let trimmed = line.trim_start();
    let body = if kind == MarkdownTextKind::List {
        split_markdown_list_prefix(trimmed).1
    } else {
        trimmed
    };
    markdown_line_spans(
        line,
        &parse_inline_spans_with_options(body, false, inline_options),
        kind,
        dim,
    )
}

fn markdown_line_spans(
    line: &str,
    base_spans: &[InlineSpan],
    kind: MarkdownTextKind,
    dim: bool,
) -> Vec<InlineSpan> {
    let trimmed = line.trim_start();
    let leading_ws = &line[..line.len() - trimmed.len()];
    let mut line_spans = Vec::new();

    if kind == MarkdownTextKind::Heading {
        line_spans.extend(base_spans.iter().cloned().map(|mut span| {
            span.style.bold = true;
            span.style.dim |= dim;
            span.style.group = Some(intern("SmeltHeading"));
            span
        }));
    } else if kind == MarkdownTextKind::BlockQuote {
        line_spans.extend(base_spans.iter().cloned().map(|mut span| {
            span.style.dim = true;
            span.style.italic = true;
            span
        }));
    } else {
        let prefix = if kind == MarkdownTextKind::List {
            split_markdown_list_prefix(trimmed).0
        } else {
            ""
        };
        if !leading_ws.is_empty() {
            line_spans.push(InlineSpan {
                text: leading_ws.to_string(),
                style: InlineStyle {
                    dim,
                    ..Default::default()
                },
                meta: Default::default(),
                break_policy: BreakPolicy::Normal,
            });
        }
        if !prefix.is_empty() {
            line_spans.push(InlineSpan {
                text: prefix.to_string(),
                style: InlineStyle {
                    dim: true,
                    ..Default::default()
                },
                meta: Default::default(),
                break_policy: BreakPolicy::Normal,
            });
        }
        line_spans.extend(base_spans.iter().cloned().map(|mut span| {
            span.style.dim |= dim;
            span
        }));
    }

    line_spans
}

fn render_horizontal_rule(
    out: &mut LineBuilder,
    bctx: Option<&smelt_core::content::BoxContext>,
    indent: &str,
) -> u16 {
    let hr = "─".repeat(3);

    if let Some(b) = bctx {
        b.print_left(out);
    } else if !indent.is_empty() {
        out.print(indent);
    }

    out.push_dim();
    out.print_with_meta(&hr, smelt_core::buffer::SpanMeta::copy_as("---"));
    out.pop_style();

    if let Some(b) = bctx {
        b.print_right(out, 3);
    }

    out.newline();
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_core::content::builder::test_util::render_test;

    #[test]
    fn markdown_range_matches_full_render_slice() {
        let md = "# Heading\n\nParagraph with enough words to wrap over multiple terminal rows at a narrow width.\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n\n| col | value |\n| --- | --- |\n| a | table cell that also wraps over several rows |\n\nAfter table text.";
        let full = render_test(36, |sink| {
            render_markdown_inner(sink, md, 36, "", false, None);
        });
        let full_rows: Vec<&str> = full.lines.iter().map(|line| line.text.as_str()).collect();

        for start in 0..full_rows.len() {
            for count in [1usize, 4, 8] {
                let range = render_test(36, |sink| {
                    render_markdown_inner_range_with_options(
                        sink,
                        md,
                        36,
                        "",
                        false,
                        None,
                        &InlineOptions::default(),
                        start,
                        count,
                    );
                });
                let range_rows: Vec<&str> =
                    range.lines.iter().map(|line| line.text.as_str()).collect();
                let end = start.saturating_add(count).min(full_rows.len());
                assert_eq!(
                    range_rows,
                    full_rows[start..end],
                    "start={start} count={count}"
                );
            }
        }
    }

    #[test]
    fn markdown_arc_accounting_includes_the_header_for_empty_nodes() {
        let nodes: Arc<[MarkdownNode]> = Arc::from([]);

        assert_eq!(markdown_arc_retained_bytes(&nodes), arc_header_bytes());
    }

    #[test]
    fn markdown_cache_evicts_an_oversize_many_span_entry() {
        MARKDOWN_PARSE_CACHE.with_borrow_mut(MarkdownParseCache::clear);
        let source = "**x** ".repeat(120_000);
        let parsed = parse_markdown_cached(&source, &InlineOptions::default());

        assert!(!parsed.nodes.is_empty());
        MARKDOWN_PARSE_CACHE.with_borrow(|cache| {
            assert!(cache.entries.is_empty());
            assert_eq!(cache.retained_bytes, 0);
        });
    }

    #[test]
    fn retained_markdown_cache_updates_after_lazy_parse_and_layout_growth() {
        RETAINED_MARKDOWN_PARSE_CACHE.with_borrow_mut(RetainedMarkdownParseCache::clear);
        let content = TranscriptContent::from(
            (0..128)
                .map(|index| {
                    format!(
                        "Paragraph {index} with **bold** and [link](https://example.test/{index})."
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
        );
        let options = InlineOptions::default();
        let parsed = parse_retained_markdown_cached(&content, &options);
        let before = RETAINED_MARKDOWN_PARSE_CACHE.with_borrow(|cache| cache.retained_bytes);

        measure_retained_markdown_inner_with_options(&content, 40, "", false, None, &options);

        RETAINED_MARKDOWN_PARSE_CACHE.with_borrow(|cache| {
            assert_eq!(cache.entries.len(), 1);
            assert!(cache.retained_bytes > before);
            assert_eq!(cache.retained_bytes, cache.entries[0].retained_bytes);
            assert!(cache.measured_retained_bytes() <= MARKDOWN_PARSE_CACHE_BYTES);
        });
        assert!(!parsed.borrow().layouts.is_empty());
    }

    #[test]
    fn retained_markdown_cache_evicts_after_oversize_table_materializes() {
        RETAINED_MARKDOWN_PARSE_CACHE.with_borrow_mut(RetainedMarkdownParseCache::clear);
        let source = format!(
            "| value |\n| --- |\n| {} |\n\nsuffix",
            "x".repeat(MARKDOWN_PARSE_CACHE_BYTES + 1)
        );
        let content = TranscriptContent::from(source);
        let options = InlineOptions::default();
        let parsed = parse_retained_markdown_cached(&content, &options);
        assert_eq!(
            RETAINED_MARKDOWN_PARSE_CACHE.with_borrow(|cache| cache.entries.len()),
            1
        );

        let read = content.read();
        let source = MarkdownSource::Retained(&read);
        let materialized_nodes = {
            let parsed = parsed.borrow();
            assert!(!parsed.completed.is_empty());
            parsed.completed[0].nodes(source, &options).len()
        };
        drop(read);
        refresh_retained_markdown_cache_weight(&parsed);

        assert!(materialized_nodes > 0);
        RETAINED_MARKDOWN_PARSE_CACHE.with_borrow(|cache| {
            assert!(cache.entries.is_empty());
            assert_eq!(cache.retained_bytes, 0);
        });
        assert!(!parsed.borrow().completed.is_empty());
    }

    #[test]
    fn retained_markdown_reuses_completed_prefix_nodes_after_suffix_appends() {
        RETAINED_MARKDOWN_PARSE_CACHE.with_borrow_mut(RetainedMarkdownParseCache::clear);
        let content = TranscriptContent::from("# café\n\nmutable".to_string());
        let options = InlineOptions::default();

        let first = parse_retained_markdown_cached(&content, &options);
        let first_prefix = {
            let parsed = first.borrow();
            assert_eq!(parsed.completed.len(), 1);
            Rc::clone(&parsed.completed[0])
        };
        measure_retained_markdown_inner_with_options(&content, 30, "", false, None, &options);
        let first_completed_states = {
            let parsed = first.borrow();
            parsed.layouts[0].completed_states.clone()
        };

        content.append_owned(" suffix".to_string());
        let second = parse_retained_markdown_cached(&content, &options);
        assert!(Rc::ptr_eq(&first, &second));
        measure_retained_markdown_inner_with_options(&content, 30, "", false, None, &options);
        {
            let parsed = second.borrow();
            assert_eq!(parsed.completed.len(), 1);
            assert!(Rc::ptr_eq(&first_prefix, &parsed.completed[0]));
            assert_eq!(parsed.layouts[0].completed_states, first_completed_states);
        }

        content.append_owned("\n\nnext".to_string());
        let third = parse_retained_markdown_cached(&content, &options);
        let parsed = third.borrow();
        assert_eq!(parsed.completed.len(), 2);
        assert!(Rc::ptr_eq(&first_prefix, &parsed.completed[0]));
    }

    #[test]
    fn retained_markdown_matches_contiguous_render_across_chunk_boundaries() {
        RETAINED_MARKDOWN_PARSE_CACHE.with_borrow_mut(RetainedMarkdownParseCache::clear);
        let content = TranscriptContent::from("# café".to_string());
        content.append_owned(" heading\n\nParagraph with **bold** 東京.\n\n```rust\n".to_string());
        content.append_owned("fn main() {}\n```\n\n| col | value |\n| --- | --- |\n".to_string());
        content.append_owned("| α | β |\n\nAfter table.".to_string());
        let snapshot = content.snapshot();
        let options = InlineOptions::default();

        let contiguous = render_test(42, |sink| {
            render_markdown_inner_with_options(sink, &snapshot, 42, "", false, None, &options);
        });
        let retained = render_test(42, |sink| {
            render_retained_markdown_inner_range_with_options(
                sink,
                &content,
                42,
                "",
                false,
                None,
                &options,
                0,
                usize::MAX,
            );
        });

        assert_eq!(retained.lines.len(), contiguous.lines.len());
        for (retained, contiguous) in retained.lines.iter().zip(&contiguous.lines) {
            assert_eq!(retained.text, contiguous.text);
            assert_eq!(retained.source_text, contiguous.source_text);
            assert_eq!(
                retained.external_source_text,
                contiguous.external_source_text
            );
            assert_eq!(retained.soft_wrapped, contiguous.soft_wrapped);
            assert_eq!(retained.cell_selectable, contiguous.cell_selectable);
            assert_eq!(retained.block_selectable, contiguous.block_selectable);
            assert_eq!(retained.copy_continuation, contiguous.copy_continuation);
            assert_eq!(retained.spans.len(), contiguous.spans.len());
            for (retained, contiguous) in retained.spans.iter().zip(&contiguous.spans) {
                assert_eq!(retained.text, contiguous.text);
                assert_eq!(retained.style, contiguous.style);
                assert_eq!(retained.meta, contiguous.meta);
            }
        }
        let contiguous_rows = contiguous
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        for start in 0..contiguous_rows.len() {
            let range = render_test(42, |sink| {
                render_retained_markdown_inner_range_with_options(
                    sink, &content, 42, "", false, None, &options, start, 3,
                );
            });
            let range_rows = range
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>();
            let end = start.saturating_add(3).min(contiguous_rows.len());
            assert_eq!(range_rows, contiguous_rows[start..end], "start={start}");
        }
        assert!(retained.lines.iter().any(|line| line
            .source_text
            .as_deref()
            .is_some_and(|source| source.contains("| col |"))));
    }

    #[test]
    fn retained_markdown_constructs_match_contiguous_render() {
        let cases = [
            (
                "loose unordered list",
                "- first\n\n  continuation\n\n- second",
            ),
            (
                "loose ordered list",
                "1. first\n\n   continuation\n\n2. second",
            ),
            ("block quote", "> quote\n>\n> continued\n\noutside"),
            ("indented code", "    code one\n\n    code two\n\noutside"),
            (
                "forward reference",
                "[reference][id]\n\n[id]: https://example.com",
            ),
            (
                "backward reference",
                "[id]: https://example.com\n\n[reference][id]",
            ),
        ];
        let options = InlineOptions::default();

        for (name, source) in cases {
            let content = TranscriptContent::from(source.to_string());
            let contiguous = render_test(40, |sink| {
                render_markdown_inner_with_options(sink, source, 40, "", false, None, &options);
            });
            let retained = render_test(40, |sink| {
                render_retained_markdown_inner_range_with_options(
                    sink,
                    &content,
                    40,
                    "",
                    false,
                    None,
                    &options,
                    0,
                    usize::MAX,
                );
            });
            let contiguous_rows = contiguous
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>();
            let retained_rows = retained
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>();
            assert_eq!(retained_rows, contiguous_rows, "{name}");

            for max_rows in 0..=contiguous_rows.len().min(4) {
                for tail in [false, true] {
                    let edge = render_test(40, |sink| {
                        let window = render_retained_markdown_inner_edge_with_options(
                            sink, &content, 40, "", false, None, &options, max_rows, tail,
                        );
                        assert_eq!(window.row_count, max_rows, "{name} tail={tail}");
                        assert_eq!(
                            window.truncated,
                            contiguous_rows.len() > max_rows,
                            "{name} tail={tail}"
                        );
                    });
                    let edge_rows = edge
                        .lines
                        .iter()
                        .map(|line| line.text.as_str())
                        .collect::<Vec<_>>();
                    let expected = if tail {
                        &contiguous_rows[contiguous_rows.len().saturating_sub(max_rows)..]
                    } else {
                        &contiguous_rows[..max_rows]
                    };
                    assert_eq!(edge_rows, expected, "{name} tail={tail}");
                }
            }
        }
    }

    #[test]
    fn retained_markdown_edges_match_full_rendered_rows() {
        let content = TranscriptContent::from(
            "# Heading\n\nFirst paragraph with **bold** text.\n\n- one\n- two\n\n> quote\n\nLast paragraph."
                .to_string(),
        );
        let options = InlineOptions::default();
        let full = render_test(28, |sink| {
            render_retained_markdown_inner_range_with_options(
                sink,
                &content,
                28,
                "",
                false,
                None,
                &options,
                0,
                usize::MAX,
            );
        });
        let full_rows = full
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();

        for tail in [false, true] {
            let edge = render_test(28, |sink| {
                let window = render_retained_markdown_inner_edge_with_options(
                    sink, &content, 28, "", false, None, &options, 4, tail,
                );
                assert_eq!(window.row_count, 4);
                assert!(window.truncated);
            });
            let edge_rows = edge
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>();
            let expected = if tail {
                &full_rows[full_rows.len() - 4..]
            } else {
                &full_rows[..4]
            };
            assert_eq!(edge_rows, expected, "tail={tail}");
        }
    }

    #[test]
    fn retained_markdown_edges_do_not_build_complete_parse_or_layout_caches() {
        RETAINED_MARKDOWN_PARSE_CACHE.with_borrow_mut(RetainedMarkdownParseCache::clear);
        let content = TranscriptContent::from(
            (0..20_000)
                .map(|index| format!("Paragraph {index:05} with **bold** text.\n"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let retained_before = content.retained_bytes();
        let options = InlineOptions::default();

        let measured = measure_retained_markdown_inner_edge_with_options(
            &content, 40, "", false, None, &options, 4,
        );
        let rendered = render_test(40, |sink| {
            render_retained_markdown_inner_edge_with_options(
                sink, &content, 40, "", false, None, &options, 4, true,
            );
        });

        assert_eq!(measured.row_count, 4);
        assert!(measured.truncated);
        assert_eq!(rendered.lines.len(), 4);
        assert!(RETAINED_MARKDOWN_PARSE_CACHE.with_borrow(|cache| cache.entries.is_empty()));
        assert_eq!(content.retained_bytes(), retained_before);
    }

    #[test]
    fn markdown_line_spans_use_shared_block_markers() {
        let options = InlineOptions::default();
        let heading = fallback_markdown_line_spans(
            "#not heading",
            MarkdownTextKind::Paragraph,
            false,
            &options,
        );
        assert_ne!(heading[0].style.group, Some(intern("SmeltHeading")));

        let bullet =
            fallback_markdown_line_spans("+ item", MarkdownTextKind::List, false, &options);
        assert_eq!(bullet[0].text, "+ ");
        assert!(bullet[0].style.dim);

        let ordered =
            fallback_markdown_line_spans("12) item", MarkdownTextKind::List, false, &options);
        assert_eq!(ordered[0].text, "12) ");
        assert!(ordered[0].style.dim);
    }

    #[test]
    fn markdown_collapses_leading_blank_run_before_code_block() {
        let md = "\n\nValidation run in the new worktree:\n\n```bash\ncargo test\n```\n";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        let rows: Vec<&str> = block.lines.iter().map(|l| l.text.as_str()).collect();

        assert_eq!(rows[0], "Validation run in the new worktree:");
        assert_eq!(rows.iter().filter(|row| row.is_empty()).count(), 1);
        assert!(rows.iter().any(|row| row.contains("cargo test")));
        assert!(!rows.iter().any(|row| row.contains("``")), "rows: {rows:?}");
    }

    #[test]
    fn streamed_table_never_renders_raw_delimiter_row() {
        fn rendered_rows(history: &smelt_core::transcript_model::BlockHistory) -> Vec<String> {
            let mut rows = Vec::new();
            for index in 0..history.len() {
                if let Some(smelt_core::transcript_model::Block::Text { content }) =
                    history.materialized_block_at(index)
                {
                    let content = content.snapshot();
                    let block = render_test(80, |sink| {
                        render_markdown_inner(sink, &content, 80, "", false, None);
                    });
                    rows.extend(block.lines.into_iter().map(|line| line.text));
                }
            }
            rows
        }

        let mut parser = smelt_core::content::stream_parser::StreamParser::new();
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        let mut previous_row_count = 0;

        for (chunk, completes_table_row) in [
            ("| a | b |", false),
            ("\n|", false),
            ("---", false),
            ("|", false),
            ("---", false),
            ("|", false),
            ("\n|", false),
            (" 1 | 2 |", false),
            ("\n", true),
            ("\n|", false),
            (" 3 |", false),
            (" 4 |", false),
            ("\n", true),
        ] {
            parser.append_streaming_text(&mut transcript.history, chunk);
            let rows = rendered_rows(&transcript.history);
            assert!(
                !rows.iter().any(|row| row.contains("---")),
                "rows: {rows:?}"
            );
            assert!(!rows.iter().any(|row| row.trim() == "|"), "rows: {rows:?}");
            if !completes_table_row {
                assert_eq!(
                    rows.len(),
                    previous_row_count,
                    "chunk {chunk:?} changed rendered row count: {rows:?}"
                );
            }
            previous_row_count = rows.len();
        }
    }

    #[test]
    fn markdown_dim_survives_code_blocks() {
        let md = "before\n\n```rust\nlet x = 1;\n```\n\nafter";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", true, None);
        });

        for line in &block.lines {
            for span in &line.spans {
                if !span.text.trim().is_empty() {
                    assert!(span.style.dim, "dim missing on span '{}'", span.text);
                }
            }
        }
    }

    #[test]
    fn markdown_code_block_can_contain_shorter_fenced_block() {
        let md = "````markdown\n```rust\nfn main() {}\n```\n````";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        let rows: Vec<&str> = block.lines.iter().map(|l| l.text.as_str()).collect();

        assert!(
            rows.iter().any(|row| row.contains("```rust")),
            "rows: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("fn main()")),
            "rows: {rows:?}"
        );
        assert_eq!(rows.iter().filter(|row| row.contains("````")).count(), 0);
    }

    #[test]
    fn markdown_code_block_ignores_longer_opening_fence_line() {
        let md = "````markdown\n`````text\ninside\n`````\n````";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        let rows: Vec<&str> = block.lines.iter().map(|l| l.text.as_str()).collect();

        assert!(
            rows.iter().any(|row| row.contains("`````text")),
            "rows: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("inside")),
            "rows: {rows:?}"
        );
    }

    #[test]
    fn markdown_code_block_keeps_fence_with_trailing_text_as_content() {
        let md = "````\n```` text\ninside\n````";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        let rows: Vec<String> = block
            .lines
            .iter()
            .map(|l| l.text.trim_end().to_string())
            .collect();

        assert_eq!(rows, vec!["```` text", "inside"]);
    }

    #[test]
    fn markdown_code_block_closes_on_longer_plain_fence() {
        let md = "````\ninside\n`````\nafter";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        let rows: Vec<String> = block
            .lines
            .iter()
            .map(|l| l.text.trim_end().to_string())
            .collect();

        assert_eq!(rows, vec!["inside", "", "after"]);
    }

    #[test]
    fn markdown_adjacent_nested_code_blocks_preserve_inner_fences() {
        let md = "````\n```\n```\n````\n````\n```\nnested code block\n```\n````";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        let rows: Vec<String> = block
            .lines
            .iter()
            .map(|l| l.text.trim_end().to_string())
            .collect();

        assert_eq!(
            rows,
            vec!["```", "```", "", "```", "nested code block", "```"]
        );
    }

    #[test]
    fn rendered_table_attaches_raw_source_to_first_row() {
        let md = "| col | val |\n| --- | --- |\n| a   | 1   |\n";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        assert!(block.lines.len() >= 2);
        assert_eq!(
            block.lines[0].source_text.as_deref(),
            Some("| col | val |\n| --- | --- |\n| a   | 1   |")
        );
        for line in &block.lines {
            assert!(line.cell_selectable);
            assert!(line.block_selectable);
        }
        for line in block.lines.iter().skip(1) {
            assert!(line.copy_continuation);
            assert!(!line.soft_wrapped);
            assert!(line.source_text.is_none());
        }
    }

    #[test]
    fn rendered_table_honors_separator_alignment_markers() {
        // Generous header widths so per-column padding is visible.
        let md = "| LLLL | CCCC | RRRR |\n|:-----|:----:|-----:|\n| x | y | z |\n";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        let data_row = block
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .find(|s| s.contains('x') && s.contains('y') && s.contains('z'))
            .expect("data row");
        assert!(data_row.contains("┃ x    ┃"), "left: {data_row:?}");
        assert!(data_row.contains("┃  y   ┃"), "center: {data_row:?}");
        assert!(data_row.contains("┃    z ┃"), "right: {data_row:?}");
    }

    #[test]
    fn rendered_table_keeps_escaped_pipe_inside_code_cell() {
        let md = "| System | Mechanism | Outcome |\n|---|---|---|\n| **Smelt** | Unix `flock(LOCK_EX\\|LOCK_NB)` | Second |\n";
        let block = render_test(120, |sink| {
            render_markdown_inner(sink, md, 120, "", false, None);
        });
        let data_row = block
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .find(|s| s.contains("Smelt"))
            .expect("data row");

        assert_eq!(data_row.matches('┃').count(), 4, "row: {data_row:?}");
        assert!(data_row.contains("LOCK_NB"), "row: {data_row:?}");
    }
}
