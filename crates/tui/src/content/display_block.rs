use crate::content::transcript_parsers::{
    compacted, exec, mode, process_status, text, thinking, tool_call, user,
};
use crate::smelt_edit::{Buffer, Theme};
use smelt_core::content::builder::{LineBuilder, Outcome};
use smelt_core::content::code_block::{measure_code_block, parse_code_block, CodeBlock};
use smelt_core::content::highlight::render_code_block;
use smelt_core::theme::intern;
use smelt_core::transcript_model::{Block, BlockHistory, BlockId, LayoutKey, ToolState, ViewState};
use std::collections::{HashMap, HashSet};

pub(crate) const DISPLAY_RENDERER_VERSION: u64 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DisplayCacheKey {
    pub(crate) content_hash: u64,
    pub(crate) sidecar_hash: u64,
    pub(crate) renderer_version: u64,
}

impl DisplayCacheKey {
    pub(crate) fn new(content_hash: u64, sidecar_hash: u64) -> Self {
        Self {
            content_hash,
            sidecar_hash,
            renderer_version: DISPLAY_RENDERER_VERSION,
        }
    }

    fn from_layout_key(key: LayoutKey) -> Self {
        Self::new(key.content_hash, key.sidecar_hash)
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum DisplayBlock {
    User {
        text: String,
        image_labels: Vec<String>,
    },
    Mode {
        text: String,
        icon: String,
        hl_group: String,
    },
    ProcessStatus {
        text: String,
    },
    Thinking {
        content: String,
    },
    Text {
        content: String,
    },
    CodeLine {
        content: String,
        lang: String,
        code: CodeBlock,
    },
    ToolCall {
        call_id: String,
        name: String,
        summary: protocol::StyledLines,
        args: HashMap<String, serde_json::Value>,
        state: ToolState,
    },
    Exec {
        command: String,
        output: String,
    },
    Compacted {
        summary: String,
    },
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct DisplayCacheEntry {
    pub(crate) id: BlockId,
    pub(crate) key: DisplayCacheKey,
    pub(crate) block: DisplayBlock,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct DisplayRowIndexEntry {
    pub(crate) width: u16,
    pub(crate) show_thinking: bool,
    pub(crate) nodes: Vec<DisplayRowIndexNode>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct DisplayRowIndexNode {
    pub(crate) id: BlockId,
    pub(crate) key: LayoutKey,
    pub(crate) exact_height: u64,
}

pub(crate) struct CompileJob {
    id: BlockId,
    key: DisplayCacheKey,
    block: Block,
    state: Option<ToolState>,
}

impl CompileJob {
    pub(crate) fn compile(self) -> (BlockId, DisplayCacheKey, DisplayBlock) {
        let Self {
            id,
            key,
            block,
            state,
        } = self;
        (id, key, compile_block(&block, state.as_ref()))
    }
}

impl DisplayBlock {
    fn matches_block(&self, block: &Block) -> bool {
        match (self, block) {
            (
                Self::User { text, image_labels },
                Block::User {
                    text: block_text,
                    image_labels: block_image_labels,
                },
            ) => text == block_text && image_labels == block_image_labels,
            (
                Self::Mode {
                    text,
                    icon,
                    hl_group,
                },
                Block::Mode {
                    text: block_text,
                    icon: block_icon,
                    hl_group: block_hl_group,
                },
            ) => text == block_text && icon == block_icon && hl_group == block_hl_group,
            (Self::ProcessStatus { text }, Block::ProcessStatus { text: block_text }) => {
                text == block_text
            }
            (
                Self::Thinking { content },
                Block::Thinking {
                    content: block_content,
                },
            ) => content == block_content,
            (
                Self::Text { content },
                Block::Text {
                    content: block_content,
                },
            ) => content == block_content,
            (
                Self::CodeLine {
                    content,
                    lang,
                    code,
                },
                Block::CodeLine {
                    content: block_content,
                    lang: block_lang,
                },
            ) => {
                content == block_content
                    && lang == block_lang
                    && code == &parse_code_block(&[content.as_str()], lang)
            }
            (
                Self::ToolCall {
                    call_id,
                    name,
                    summary,
                    args,
                    ..
                },
                Block::ToolCall {
                    call_id: block_call_id,
                    name: block_name,
                    summary: block_summary,
                    args: block_args,
                },
            ) => {
                call_id == block_call_id
                    && name == block_name
                    && summary == block_summary
                    && args == block_args
            }
            (
                Self::Exec { command, output },
                Block::Exec {
                    command: block_command,
                    output: block_output,
                },
            ) => command == block_command && output == block_output,
            (
                Self::Compacted { summary },
                Block::Compacted {
                    summary: block_summary,
                },
            ) => summary == block_summary,
            _ => false,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct MeasureCtx {
    pub width: u16,
    pub show_thinking: bool,
    pub view_state: ViewState,
}

#[derive(Clone, Copy)]
pub(crate) struct RenderCtx<'a> {
    pub width: u16,
    pub show_thinking: bool,
    pub view_state: ViewState,
    pub theme: &'a Theme,
}

struct CachedDisplayBlock {
    key: DisplayCacheKey,
    block: DisplayBlock,
}

#[derive(Default)]
pub(crate) struct DisplayModel {
    blocks: HashMap<BlockId, CachedDisplayBlock>,
}

impl DisplayModel {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.blocks.len()
    }

    pub(crate) fn hydrate_many(
        &mut self,
        history: &mut BlockHistory,
        entries: Vec<DisplayCacheEntry>,
    ) -> usize {
        smelt_perf::perf::record_value(
            "transcript:display_model:hydrate_requested",
            entries.len() as u64,
        );
        let mut hydrated = 0;
        for entry in entries {
            if self.hydrate_one(history, entry) {
                hydrated += 1;
            }
        }
        hydrated
    }

    fn hydrate_one(&mut self, history: &mut BlockHistory, entry: DisplayCacheEntry) -> bool {
        smelt_perf::perf::record_value("transcript:display_model:hydrate_attempt", 1);
        let Some(current_block) = history.blocks.get(&entry.id).cloned() else {
            smelt_perf::perf::record_value(
                "transcript:display_model:hydrate_reject:missing_block",
                1,
            );
            return false;
        };
        if !entry.block.matches_block(&current_block) {
            smelt_perf::perf::record_value(
                "transcript:display_model:hydrate_reject:block_mismatch",
                1,
            );
            return false;
        }

        if let DisplayBlock::ToolCall { call_id, state, .. } = &entry.block {
            let Some(current_state) = history.tool_state(call_id) else {
                smelt_perf::perf::record_value(
                    "transcript:display_model:hydrate_reject:missing_tool_state",
                    1,
                );
                return false;
            };
            let cached_body = state.body.clone();
            let mut candidate = current_state.clone();
            candidate.body = cached_body.clone();
            if candidate.display_hash() != entry.key.sidecar_hash {
                smelt_perf::perf::record_value(
                    "transcript:display_model:hydrate_reject:tool_sidecar_hash",
                    1,
                );
                return false;
            }
            history.update_tool_state(call_id, |state| {
                state.body = cached_body;
            });
        }

        let key = history.resolve_key(
            entry.id,
            LayoutKey {
                width: 0,
                show_thinking: false,
                view_state: ViewState::Expanded,
                content_hash: 0,
                sidecar_hash: 0,
            },
        );
        let display_key = DisplayCacheKey::from_layout_key(key);
        if display_key != entry.key {
            smelt_perf::perf::record_value(
                "transcript:display_model:hydrate_reject:key_mismatch",
                1,
            );
            return false;
        }

        self.blocks.insert(
            entry.id,
            CachedDisplayBlock {
                key: display_key,
                block: entry.block,
            },
        );
        smelt_perf::perf::record_value("transcript:display_model:hydrate_ok", 1);
        true
    }

    pub(crate) fn cache_entries(&self, order: &[BlockId]) -> Vec<DisplayCacheEntry> {
        order
            .iter()
            .filter_map(|id| {
                self.blocks.get(id).map(|cached| DisplayCacheEntry {
                    id: *id,
                    key: cached.key,
                    block: cached.block.clone(),
                })
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn ensure_many(
        &mut self,
        history: &BlockHistory,
        ids: &[BlockId],
        keys: &[LayoutKey],
    ) -> usize {
        let jobs =
            self.collect_compile_jobs(history, ids.iter().copied().zip(keys.iter().copied()));
        let compiled = jobs.len();
        let blocks = jobs.into_iter().map(CompileJob::compile).collect();
        self.insert_compiled_blocks(blocks);
        compiled
    }

    /// Returns compile jobs for cache misses. The caller can run these jobs on
    /// the current thread or schedule them onto a worker pool, then insert the
    /// results with `insert_compiled_blocks`.
    pub(crate) fn collect_compile_jobs(
        &mut self,
        history: &BlockHistory,
        blocks: impl IntoIterator<Item = (BlockId, LayoutKey)>,
    ) -> Vec<CompileJob> {
        let _perf = smelt_perf::perf::begin("transcript:display_model:ensure_many");

        let mut jobs = Vec::new();
        let mut requested = 0;
        for (id, key) in blocks {
            requested += 1;
            let display_key = DisplayCacheKey::from_layout_key(key);
            if self
                .blocks
                .get(&id)
                .is_some_and(|cached| cached.key == display_key)
            {
                continue;
            }
            let Some(block) = history.blocks.get(&id).cloned() else {
                self.blocks.remove(&id);
                continue;
            };
            let state = match &block {
                Block::ToolCall { call_id, .. } => history.tool_state(call_id).cloned(),
                _ => None,
            };
            jobs.push(CompileJob {
                id,
                key: display_key,
                block,
                state,
            });
        }
        smelt_perf::perf::record_value("transcript:display_model:requested", requested);
        smelt_perf::perf::record_value("transcript:display_model:compiled", jobs.len() as u64);
        jobs
    }

    pub(crate) fn insert_compiled_blocks(
        &mut self,
        blocks: Vec<(BlockId, DisplayCacheKey, DisplayBlock)>,
    ) {
        for (id, key, block) in blocks {
            self.blocks.insert(id, CachedDisplayBlock { key, block });
        }
    }

    pub(crate) fn retain_order(&mut self, order: &[BlockId]) {
        let live: HashSet<BlockId> = order.iter().copied().collect();
        self.blocks.retain(|id, _| live.contains(id));
    }

    pub(crate) fn get(&self, id: BlockId, key: LayoutKey) -> Option<&DisplayBlock> {
        let display_key = DisplayCacheKey::from_layout_key(key);
        self.blocks
            .get(&id)
            .filter(|cached| cached.key == display_key)
            .map(|cached| &cached.block)
    }
}

pub(crate) fn compile_block(block: &Block, state: Option<&ToolState>) -> DisplayBlock {
    match block {
        Block::User { text, image_labels } => DisplayBlock::User {
            text: text.clone(),
            image_labels: image_labels.clone(),
        },
        Block::Mode {
            text,
            icon,
            hl_group,
        } => DisplayBlock::Mode {
            text: text.clone(),
            icon: icon.clone(),
            hl_group: hl_group.clone(),
        },
        Block::ProcessStatus { text } => DisplayBlock::ProcessStatus { text: text.clone() },
        Block::Thinking { content } => DisplayBlock::Thinking {
            content: content.clone(),
        },
        Block::Text { content } => DisplayBlock::Text {
            content: content.clone(),
        },
        Block::CodeLine { content, lang } => DisplayBlock::CodeLine {
            content: content.clone(),
            lang: lang.clone(),
            code: parse_code_block(&[content.as_str()], lang),
        },
        Block::ToolCall {
            call_id,
            name,
            summary,
            args,
        } => DisplayBlock::ToolCall {
            call_id: call_id.clone(),
            name: name.clone(),
            summary: summary.clone(),
            args: args.clone(),
            state: state
                .cloned()
                .unwrap_or_else(|| panic!("missing ToolState for tool call `{call_id}`")),
        },
        Block::Exec { command, output } => DisplayBlock::Exec {
            command: command.clone(),
            output: output.clone(),
        },
        Block::Compacted { summary } => DisplayBlock::Compacted {
            summary: summary.clone(),
        },
    }
}

pub(crate) fn measure_block(block: &DisplayBlock, ctx: MeasureCtx) -> u64 {
    let _perf = smelt_perf::perf::begin(measure_block_label(block));
    let width = ctx.width as usize;
    let expanded_rows = match block {
        DisplayBlock::User { text, .. } => user::measure(text, width) as u64,
        DisplayBlock::Mode { .. } => 1,
        DisplayBlock::ProcessStatus { text } => process_status::measure(text, width) as u64,
        DisplayBlock::Thinking { content } => {
            thinking::measure(content, width, ctx.show_thinking) as u64
        }
        DisplayBlock::Text { content } => text::measure(content, width) as u64,
        DisplayBlock::CodeLine { code, .. } => measure_code_block(code, width) as u64,
        DisplayBlock::ToolCall {
            name,
            summary,
            state,
            ..
        } => crate::content::transcript_parsers::measure_tool_height(
            name,
            summary,
            state.status,
            state.elapsed,
            state.output.as_deref(),
            state.user_message.as_deref(),
            state.body.as_ref(),
            width,
        ) as u64,
        DisplayBlock::Exec { command, output } => exec::measure(command, output, width) as u64,
        DisplayBlock::Compacted { summary } => compacted::measure(summary, width) as u64,
    };
    ctx.view_state.measured_height(expanded_rows)
}

fn measure_block_label(block: &DisplayBlock) -> &'static str {
    match block {
        DisplayBlock::User { .. } => "transcript:measure_block:user",
        DisplayBlock::Mode { .. } => "transcript:measure_block:mode",
        DisplayBlock::ProcessStatus { .. } => "transcript:measure_block:process_status",
        DisplayBlock::Thinking { .. } => "transcript:measure_block:thinking",
        DisplayBlock::Text { .. } => "transcript:measure_block:text",
        DisplayBlock::CodeLine { .. } => "transcript:measure_block:code_line",
        DisplayBlock::ToolCall { .. } => "transcript:measure_block:tool_call",
        DisplayBlock::Exec { .. } => "transcript:measure_block:exec",
        DisplayBlock::Compacted { .. } => "transcript:measure_block:compacted",
    }
}

pub(crate) fn render_block_into(
    buf: &mut Buffer,
    block: &DisplayBlock,
    ctx: RenderCtx<'_>,
) -> Outcome {
    let outcome = {
        let mut out = LineBuilder::new(buf, ctx.theme, ctx.width);
        render_expanded_block(&mut out, block, ctx.width as usize, ctx.show_thinking);
        out.finish()
    };
    apply_view_state(buf, ctx.theme, ctx.width, ctx.view_state, outcome)
}

fn render_expanded_block(
    out: &mut LineBuilder,
    block: &DisplayBlock,
    width: usize,
    show_thinking: bool,
) -> u16 {
    let _perf = smelt_perf::perf::begin(render_block_label(block));
    match block {
        DisplayBlock::User { text, image_labels } => user::render(out, text, image_labels, width),
        DisplayBlock::Mode {
            text,
            icon,
            hl_group,
        } => mode::render(out, text, icon, hl_group),
        DisplayBlock::ProcessStatus { text } => process_status::render(out, text),
        DisplayBlock::Thinking { content } => thinking::render(out, content, width, show_thinking),
        DisplayBlock::Text { content } => text::render(out, content, width),
        DisplayBlock::CodeLine { code, .. } => {
            render_code_block(out, code, width, false, None, false)
        }
        DisplayBlock::ToolCall {
            call_id,
            name,
            summary,
            args,
            state,
        } => tool_call::render(
            out,
            call_id,
            name,
            summary,
            args,
            state.status,
            state.elapsed,
            state,
            width,
        ),
        DisplayBlock::Exec { command, output } => exec::render(out, command, output, width),
        DisplayBlock::Compacted { summary } => compacted::render(out, summary, width),
    }
}

fn render_block_label(block: &DisplayBlock) -> &'static str {
    match block {
        DisplayBlock::User { .. } => "render:user",
        DisplayBlock::Mode { .. } => "render:mode",
        DisplayBlock::ProcessStatus { .. } => "render:process_status",
        DisplayBlock::Thinking { .. } => "render:thinking",
        DisplayBlock::Text { .. } => "render:text",
        DisplayBlock::CodeLine { .. } => "render:code_line",
        DisplayBlock::ToolCall { .. } => "render:tool_call",
        DisplayBlock::Exec { .. } => "render:exec",
        DisplayBlock::Compacted { .. } => "render:compacted",
    }
}

fn apply_view_state(
    buf: &mut Buffer,
    theme: &Theme,
    width: u16,
    state: ViewState,
    outcome: Outcome,
) -> Outcome {
    let total = outcome.line_count;
    let target_total = state.measured_height(total as u64) as usize;
    let start = buf.line_count().saturating_sub(total);
    match state {
        ViewState::Expanded => outcome,
        ViewState::Collapsed => {
            if state.elides_rows(total as u64) {
                let hidden = total - 1;
                buf.set_lines(start + 1, start + total, vec![]);
                let after_truncate_outcome = Outcome {
                    line_count: 1,
                    ..outcome
                };
                let with_ellipsis = append_ellipsis(
                    buf,
                    theme,
                    width,
                    &format!("… {hidden} more lines"),
                    after_truncate_outcome,
                );
                Outcome {
                    line_count: target_total,
                    ..with_ellipsis
                }
            } else {
                outcome
            }
        }
        ViewState::TrimmedHead { keep } => {
            let keep = keep as usize;
            if state.elides_rows(total as u64) {
                let hidden = total - keep;
                buf.set_lines(start + keep, start + total, vec![]);
                let after_truncate_outcome = Outcome {
                    line_count: keep,
                    ..outcome
                };
                let with_ellipsis = append_ellipsis(
                    buf,
                    theme,
                    width,
                    &format!("… {hidden} more lines"),
                    after_truncate_outcome,
                );
                Outcome {
                    line_count: target_total,
                    ..with_ellipsis
                }
            } else {
                outcome
            }
        }
        ViewState::TrimmedTail { keep } => {
            let keep = keep as usize;
            if state.elides_rows(total as u64) {
                let hidden = total - keep;
                buf.set_lines(start, start + (total - keep), vec![]);
                let mut kept_lines: Vec<String> = (0..keep)
                    .map(|i| buf.get_line(start + i).unwrap_or("").to_string())
                    .collect();
                let kept_decorations: Vec<_> = (0..keep)
                    .map(|i| buf.decoration_at(start + i).clone())
                    .collect();
                let kept_highlights: Vec<_> =
                    (0..keep).map(|i| buf.highlights_at(start + i)).collect();
                buf.set_lines(start, start + keep, vec![]);
                append_ellipsis(
                    buf,
                    theme,
                    width,
                    &format!("… {hidden} more lines above"),
                    Outcome {
                        line_count: 0,
                        ..outcome
                    },
                );
                let cur_len = buf.line_count();
                buf.set_lines(cur_len, cur_len, std::mem::take(&mut kept_lines));
                for (i, hl_list) in kept_highlights.into_iter().enumerate() {
                    let row = cur_len + i;
                    for span in hl_list {
                        buf.add_highlight_group_with_meta(
                            row,
                            span.col_start,
                            span.col_end,
                            span.hl,
                            span.meta,
                        );
                    }
                }
                for (i, dec) in kept_decorations.into_iter().enumerate() {
                    if dec != smelt_core::buffer::LineDecoration::default() {
                        buf.set_decoration(cur_len + i, dec);
                    }
                }
                Outcome {
                    line_count: target_total,
                    ..outcome
                }
            } else {
                outcome
            }
        }
    }
}

fn append_ellipsis(
    buf: &mut Buffer,
    theme: &Theme,
    width: u16,
    text: &str,
    outcome: Outcome,
) -> Outcome {
    let added = {
        let mut col = LineBuilder::new(buf, theme, width);
        col.push_dim();
        col.push_hl(intern("Comment"));
        col.print(text);
        col.pop_style();
        col.pop_style();
        col.newline();
        col.finish()
    };
    Outcome {
        line_count: outcome.line_count + added.line_count,
        was_wrapped: outcome.was_wrapped || added.was_wrapped,
        max_line_width: outcome.max_line_width.max(added.max_line_width),
        layout_width: outcome.layout_width,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smelt_edit::{BufCreateOpts, BufId};
    use smelt_core::content::block_layout::{BlockLayout, IrLeaf, TextSpec, ToolBody};
    use smelt_core::content::transcript::Transcript;
    use smelt_core::transcript_model::{ToolOutput, ToolStatus};

    fn base_key(history: &BlockHistory, id: BlockId) -> LayoutKey {
        history.resolve_key(
            id,
            LayoutKey {
                width: 80,
                show_thinking: false,
                view_state: ViewState::Expanded,
                content_hash: 0,
                sidecar_hash: 0,
            },
        )
    }

    fn rendered_rows(block: &DisplayBlock, width: u16, show_thinking: bool) -> u64 {
        let theme = Theme::default();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        render_block_into(
            &mut buf,
            block,
            RenderCtx {
                width,
                show_thinking,
                view_state: ViewState::Expanded,
                theme: &theme,
            },
        )
        .line_count as u64
    }

    fn measured_rows(block: &DisplayBlock, width: u16, show_thinking: bool) -> u64 {
        measure_block(
            block,
            MeasureCtx {
                width,
                show_thinking,
                view_state: ViewState::Expanded,
            },
        )
    }

    #[test]
    fn legacy_measurement_matches_rendered_rows() {
        let blocks = [
            Block::Text {
                content: "# Heading\n\nParagraph with **bold** text that wraps across several rows at narrow widths.\n\n```rust\nfn main() { println!(\"hello\"); }\n```\n\n| col | val |\n| --- | --- |\n| a | table cell that wraps |"
                    .into(),
            },
            Block::User {
                text: "Please inspect @crates/tui/src/content/display_block.rs and this long line that wraps."
                    .into(),
                image_labels: vec![],
            },
            Block::ProcessStatus {
                text: "running a long process status that wraps on narrow terminals".into(),
            },
            Block::Thinking {
                content: "**Plan**\nThink through a long line that wraps in expanded thinking mode.".into(),
            },
            Block::Exec {
                command: "echo a very long shell command that wraps".into(),
                output: "output line that is also long enough to wrap in the transcript".into(),
            },
            Block::Compacted {
                summary: "A compacted **summary** with enough text to wrap.".into(),
            },
            Block::Mode {
                text: "plan".into(),
                icon: "◈ ".into(),
                hl_group: "SmeltAccent".into(),
            },
        ];

        for block in blocks {
            let display = compile_block(&block, None);
            for show_thinking in [false, true] {
                assert_eq!(
                    measured_rows(&display, 36, show_thinking),
                    rendered_rows(&display, 36, show_thinking),
                    "measurement mismatch for {block:?}, show_thinking={show_thinking}"
                );
            }
        }
    }

    #[test]
    fn hydrated_cache_entries_avoid_recompile() {
        let mut transcript = Transcript::new();
        transcript.push(Block::CodeLine {
            content: "fn main() {}".into(),
            lang: "rust".into(),
        });
        let id = transcript.history.order[0];
        let key = base_key(&transcript.history, id);

        let mut model = DisplayModel::new();
        assert_eq!(model.ensure_many(&transcript.history, &[id], &[key]), 1);
        let entries = model.cache_entries(&transcript.history.order);

        let mut hydrated = DisplayModel::new();
        assert_eq!(hydrated.hydrate_many(&mut transcript.history, entries), 1);
        assert_eq!(hydrated.ensure_many(&transcript.history, &[id], &[key]), 0);
    }

    #[test]
    fn hydrated_cache_rejects_mismatched_display_variant() {
        let mut transcript = Transcript::new();
        transcript.push(Block::User {
            text: "hello".into(),
            image_labels: vec![],
        });
        let id = transcript.history.order[0];
        let current = transcript.history.blocks.get(&id).unwrap();
        let key = base_key(&transcript.history, id);
        let entries = vec![DisplayCacheEntry {
            id,
            key: DisplayCacheKey::new(current.content_hash(), 0),
            block: DisplayBlock::Text {
                content: "hello".into(),
            },
        }];

        let mut hydrated = DisplayModel::new();
        assert_eq!(hydrated.hydrate_many(&mut transcript.history, entries), 0);
        assert_eq!(hydrated.ensure_many(&transcript.history, &[id], &[key]), 1);
    }

    #[test]
    fn hydrated_tool_entry_installs_cached_body() {
        let block = Block::ToolCall {
            call_id: "call-1".into(),
            name: "write_file".into(),
            summary: "write file".into(),
            args: Default::default(),
        };
        let body = ToolBody::Layout(BlockLayout::Leaf(IrLeaf::Text(TextSpec {
            content: "cached body".into(),
            hl_group: None,
        })));

        let mut warm = Transcript::new();
        warm.push_tool_call(
            block.clone(),
            ToolState {
                status: ToolStatus::Ok,
                elapsed: None,
                output: Some(Box::new(ToolOutput {
                    content: "ok".into(),
                    is_error: false,
                    metadata: None,
                })),
                user_message: None,
                body: Some(body),
            },
        );
        let id = warm.history.order[0];
        let key = base_key(&warm.history, id);
        let mut model = DisplayModel::new();
        assert_eq!(model.ensure_many(&warm.history, &[id], &[key]), 1);
        let entries = model.cache_entries(&warm.history.order);

        let mut cold = Transcript::new();
        cold.push_tool_call(
            block,
            ToolState {
                status: ToolStatus::Ok,
                elapsed: None,
                output: Some(Box::new(ToolOutput {
                    content: "ok".into(),
                    is_error: false,
                    metadata: None,
                })),
                user_message: None,
                body: None,
            },
        );
        let mut hydrated = DisplayModel::new();
        assert_eq!(hydrated.hydrate_many(&mut cold.history, entries), 1);
        assert!(cold.history.tool_state("call-1").unwrap().body.is_some());
        let cold_key = base_key(&cold.history, id);
        assert_eq!(hydrated.ensure_many(&cold.history, &[id], &[cold_key]), 0);
    }
}
