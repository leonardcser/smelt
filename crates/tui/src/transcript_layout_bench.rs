//! Layout and resume benchmarks for the transcript projection.
#![allow(clippy::all)]

use smelt_edit::*;
use smelt_transcript::content::transcript::Transcript;
use smelt_transcript::transcript_model::{Block, BlockHistory, ToolOutput, ToolState, ToolStatus};
use smelt_view::transcript_buf::*;

fn test_lua() -> smelt_core::lua::runtime::LuaRuntime {
    let lua = smelt_core::lua::runtime::LuaRuntime::new();
    let clock: std::sync::Arc<dyn engine::clock::Clock> = std::sync::Arc::new(
        engine::clock::VirtualClock::new(std::time::Instant::now(), std::time::SystemTime::now()),
    );
    lua.shared().set_clock(clock);
    lua
}

#[allow(clippy::too_many_arguments)]
fn project_with_lua(
    projection: &mut TranscriptProjection,
    lua: &smelt_core::lua::runtime::LuaRuntime,
    buf: &mut Buffer,
    history: &mut BlockHistory,
    width: u16,
    theme: &Theme,
    scroll_target: ScrollTarget,
    viewport_rows: u16,
) -> MaterializedRows {
    let plan = projection.plan_projection_measured(
        lua,
        history,
        width,
        theme,
        scroll_target,
        viewport_rows,
    );
    projection.project_planned(lua, buf, history, theme, plan)
}

fn push_large_mixed_transcript_fixture(transcript: &mut Transcript, target_bytes: usize) -> usize {
    let mut approx_bytes = 0usize;
    let mut i = 0usize;
    while approx_bytes < target_bytes {
        let user = format!(
            "Investigate transcript layout batch {i}. {}",
            "preserve exact copy navigation scrollbar resize preview ".repeat(8)
        );
        approx_bytes += user.len();
        transcript.push(Block::User {
            text: user,
            image_labels: vec![],
            command: false,
            sent_at_ms: None,
        });

        let markdown = large_mixed_markdown_payload(i);
        approx_bytes += markdown.len();
        transcript.push(Block::Text {
            content: markdown.into(),
        });

        if i.is_multiple_of(5) {
            let reasoning = format!(
                "{}\n{}",
                "consider cached width-independent measurement ".repeat(20),
                "validate row-count and copy-source equivalence ".repeat(20)
            );
            approx_bytes += reasoning.len();
            transcript.push(Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                content: reasoning.into(),
                kind: protocol::ReasoningKind::Raw,
            });
        }

        if i.is_multiple_of(7) {
            let command = format!("python scripts/analyze_layout.py --batch {i}");
            let output = (0..24)
                .map(|j| {
                    format!(
                        "result {i}.{j}: {}",
                        "tool output wraps and truncates ".repeat(10)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            approx_bytes += command.len() + output.len();
            transcript.push(Block::Exec {
                command,
                output: output.into(),
            });
        }

        if i.is_multiple_of(11) {
            let call_id = format!("mixed-fixture-call-{i}");
            let output = (0..80)
                .map(|j| {
                    format!(
                        "tool line {i}.{j}: {}",
                        "visible materialization ".repeat(12)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            approx_bytes += output.len();
            transcript.push_tool_call(
                Block::ToolCall {
                    call_id: call_id.clone(),
                    name: "edit_file".into(),
                    summary: protocol::StyledLines::from_plain(format!("edit mixed_{i}.rs")),
                    args: std::collections::HashMap::new().into(),
                },
                ToolState {
                    status: ToolStatus::Ok,
                    elapsed: Some(std::time::Duration::from_millis(1_250)),
                    called_at_ms: None,
                    elapsed_active: false,
                    output: Some(Box::new(ToolOutput {
                        content: output.into(),
                        is_error: false,
                        metadata: None,
                        content_fields: Vec::new(),
                    })),
                    user_message: None,
                    preview_output: None,
                },
            );
        }

        i += 1;
    }
    approx_bytes
}

fn large_mixed_markdown_payload(i: usize) -> String {
    format!(
            "# Batch {i}\n\n{}\n\n| file | rows | notes |\n| --- | ---: | --- |\n| transcript_{i}.rs | {} | {} |\n| render_{i}.rs | {} | {} |\n\n```rust\nfn batch_{i}() {{\n    let rows = {};\n    println!(\"{{rows}}\");\n}}\n```\n\n- {}\n- {}",
            "markdown paragraphs with inline `code`, **bold spans**, links, and wrap pressure ".repeat(18),
            i * 3 + 1,
            "table cells wrap under preview width ".repeat(8),
            i * 3 + 2,
            "copy source must remain markdown exact ".repeat(8),
            i * 17,
            "resize should preserve visible block anchors ".repeat(10),
            "first render should not materialize irrelevant rows ".repeat(10),
        )
}

fn approx_history_bytes(history: &BlockHistory) -> usize {
    let mut bytes = 0usize;
    for id in &history.order {
        if let Some(block) = history.block(*id) {
            bytes += block.raw_text().map_or(0, |text| text.len());
            if let Block::ToolCall { .. } = block {
                if let Some(state) = history.tool_state(*id) {
                    bytes += state
                        .output
                        .as_ref()
                        .map_or(0, |output| output.content.len());
                }
            }
        }
    }
    bytes
}

fn push_markdown_heavy_transcript_fixture(
    transcript: &mut Transcript,
    target_bytes: usize,
) -> usize {
    let mut approx_bytes = 0usize;
    let mut i = 0usize;
    while approx_bytes < target_bytes {
        let content = format!(
                "# Markdown-heavy document {i}\n\n{}\n\n## Table\n\n| column | value | notes |\n| --- | ---: | --- |\n| alpha | {} | {} |\n| beta | {} | {} |\n\n## Code\n\n```rust\nfn markdown_heavy_{i}() -> usize {{\n    let mut total = 0;\n    for n in 0..{} {{ total += n; }}\n    total\n}}\n```\n\n{}\n\n{}",
                "Paragraph with links, `inline code`, emphasis, and wrap pressure. ".repeat(42),
                i * 11 + 1,
                "table cells intentionally contain enough content to wrap at narrow widths ".repeat(18),
                i * 11 + 2,
                "copy/search/source exactness must survive markdown measurement ".repeat(18),
                i % 127 + 32,
                "- bullet item with long content ".repeat(44),
                "> quoted reasoning line ".repeat(36),
            );
        approx_bytes += content.len();
        transcript.push(Block::Text {
            content: content.into(),
        });
        i += 1;
    }
    approx_bytes
}

fn push_tool_output_heavy_transcript_fixture(
    transcript: &mut Transcript,
    target_bytes: usize,
) -> usize {
    let mut approx_bytes = 0usize;
    let mut i = 0usize;
    while approx_bytes < target_bytes {
        let call_id = format!("tool-output-heavy-{i}");
        let command = format!("cargo test package_{i} -- --nocapture");
        let output = (0..160)
            .map(|j| {
                format!(
                    "tool output line {i}.{j}: {}",
                    "ansi-free terminal output wraps exactly and keeps tail caps honest ".repeat(8)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        approx_bytes += command.len() + output.len();
        transcript.push_tool_call(
            Block::ToolCall {
                call_id: call_id.clone(),
                name: "bash".into(),
                summary: protocol::StyledLines::from_plain(command),
                args: std::collections::HashMap::new().into(),
            },
            ToolState {
                status: ToolStatus::Ok,
                elapsed: Some(std::time::Duration::from_millis(2_000 + i as u64)),
                called_at_ms: None,
                elapsed_active: false,
                output: Some(Box::new(ToolOutput {
                    content: output.into(),
                    is_error: false,
                    metadata: None,
                    content_fields: Vec::new(),
                })),
                user_message: None,
                preview_output: None,
            },
        );
        i += 1;
    }
    approx_bytes
}

fn push_many_tiny_blocks_transcript_fixture(
    transcript: &mut Transcript,
    target_bytes: usize,
) -> usize {
    let mut approx_bytes = 0usize;
    let mut i = 0usize;
    while approx_bytes < target_bytes {
        let text = format!("tiny block {i} alpha beta gamma");
        approx_bytes += text.len();
        match i % 6 {
            0 => transcript.push(Block::User {
                text,
                image_labels: vec![],
                command: false,
                sent_at_ms: None,
            }),
            1 => transcript.push(Block::Text {
                content: text.into(),
            }),
            2 => transcript.push(Block::CodeLine {
                content: format!("let tiny_{i} = {i};"),
                lang: "rust".into(),
            }),
            3 => transcript.push(Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                content: text.into(),
                kind: protocol::ReasoningKind::Raw,
            }),
            4 => transcript.push(Block::Compacted {
                summary: text,
                kept: Vec::new(),
            }),
            _ => transcript.push(Block::ProcessStatus {
                text,
                event: None,
                output: None,
            }),
        }
        i += 1;
    }
    approx_bytes
}

fn push_few_huge_blocks_transcript_fixture(
    transcript: &mut Transcript,
    target_bytes: usize,
) -> usize {
    let mut approx_bytes = 0usize;
    let mut i = 0usize;
    while approx_bytes < target_bytes {
        let content = format!(
            "# Huge block {i}\n\n{}\n\n```text\n{}\n```\n\n{}",
            "large paragraph with wrapping pressure and markdown spans `code` **bold** "
                .repeat(900),
            "preformatted output still contributes exact rows without visible materialization\n"
                .repeat(420),
            "closing paragraph ".repeat(700),
        );
        approx_bytes += content.len();
        transcript.push(Block::Text {
            content: content.into(),
        });
        i += 1;
    }
    approx_bytes
}

#[derive(Clone, Copy)]
struct TranscriptBenchWorkload {
    name: &'static str,
    target_bytes: usize,
    build: fn(&mut Transcript, usize) -> usize,
}

#[derive(Clone, Copy, Debug)]
struct TranscriptBenchSample {
    input_bytes: usize,
    generated_bytes: usize,
    blocks: usize,
    total_rows: RowIndex,
    first_ms: f64,
    resize_ms: f64,
    theme_ms: f64,
    scroll12_ms: f64,
    visible_ms: f64,
    copy_ms: f64,
    append_ms: f64,
    no_cache_ms: f64,
    allocs: u64,
    bytes_allocated: u64,
    alloc_current_bytes: usize,
    alloc_peak_bytes: usize,
    visible_rows: usize,
    copied_rows: RowIndex,
    appended_rows: RowIndex,
    scroll_materialized_rows: u64,
    first_counters: TranscriptProjectionCounters,
    resize_counters: TranscriptProjectionCounters,
    theme_counters: TranscriptProjectionCounters,
    scroll_counters: TranscriptProjectionCounters,
    visible_counters: TranscriptProjectionCounters,
    copy_counters: TranscriptProjectionCounters,
    append_counters: TranscriptProjectionCounters,
    no_cache_counters: TranscriptProjectionCounters,
}

use smelt_test_support::bench::Stats as MetricStats;

fn elapsed_ms(elapsed: std::time::Duration) -> f64 {
    elapsed.as_secs_f64() * 1_000.0
}

fn counters_json(counters: TranscriptProjectionCounters) -> String {
    format!(
            "{{\"full_row_builds\":{},\"layout_cache\":{},\"exact_height_measured_blocks\":{},\"range_materialized_blocks\":{},\"max_range_materialized_blocks\":{},\"range_materialized_rows\":{},\"max_range_materialized_rows\":{}}}",
            counters.full_row_builds,
            counters.layout_cache,
            counters.exact_height_measured_blocks,
            counters.range_materialized_blocks,
            counters.max_range_materialized_blocks,
            counters.range_materialized_rows,
            counters.max_range_materialized_rows,
        )
}

fn selectable_copy_range_from_rows(
    start_row: RowIndex,
    rows: &DisplayRows,
    total_rows: RowIndex,
) -> Option<DocRange> {
    rows.rows.iter().enumerate().find_map(|(offset, row)| {
        let selectable = row
            .selectable_ranges
            .iter()
            .find(|range| range.start < range.end)?;
        let row_idx = start_row.saturating_add(offset as RowIndex);
        Some(DocRange {
            start: smelt_edit::DocPosition {
                row: row_idx,
                byte_col: selectable.start,
            },
            end: smelt_edit::DocPosition {
                row: row_idx.saturating_add(79).min(total_rows.saturating_sub(1)),
                byte_col: usize::MAX,
            },
        })
    })
}

fn find_selectable_copy_range(
    projection: &mut TranscriptProjection,
    lua: &smelt_core::lua::runtime::LuaRuntime,
    history: &mut BlockHistory,
    width: u16,
    theme: &Theme,
    preferred_row: RowIndex,
    total_rows: RowIndex,
) -> Option<DocRange> {
    if total_rows == 0 {
        return None;
    }
    let chunk = 80;
    let start = preferred_row.min(total_rows.saturating_sub(1));
    for row in (start..total_rows).step_by(chunk as usize) {
        let rows = projection.display_rows_for_range(
            lua,
            history,
            width,
            theme,
            row..row.saturating_add(chunk).min(total_rows),
        );
        if let Some(range) = selectable_copy_range_from_rows(row, &rows, total_rows) {
            if !projection
                .copy_range(lua, history, width, theme, range)
                .kill_ring
                .is_empty()
            {
                return Some(range);
            }
        }
    }
    let mut row = start;
    while row > 0 {
        row = row.saturating_sub(chunk).min(row);
        let rows = projection.display_rows_for_range(
            lua,
            history,
            width,
            theme,
            row..row.saturating_add(chunk).min(total_rows),
        );
        if let Some(range) = selectable_copy_range_from_rows(row, &rows, total_rows) {
            if !projection
                .copy_range(lua, history, width, theme, range)
                .kill_ring
                .is_empty()
            {
                return Some(range);
            }
        }
    }
    None
}

fn perf_value_max(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> u64 {
    snapshot
        .values
        .iter()
        .find(|row| row.label == label)
        .map(|row| row.max)
        .unwrap_or(0)
}

fn assert_resume_tail_perf_gates(snapshot: &smelt_perf::perf::Snapshot, history_items: usize) {
    for label in [
        "store:history:read_all",
        "store:history:read_all_rows",
        "store:session:load_full_snapshot",
        "store:session:full_snapshot_rows_read",
        "store:transcript:read_records_full",
        "store:transcript:records_full_loaded",
        "transcript:build_from_session:history_items",
    ] {
        let value = perf_value_max(snapshot, label);
        assert_eq!(
            value, 0,
            "display-only resume recorded {label}={value}, expected no full-session work"
        );
    }
    let loaded = perf_value_max(snapshot, "store:transcript:records_loaded");
    assert!(
        loaded <= 256,
        "display-only resume loaded {loaded} records from {history_items} history items"
    );
    let record_total = perf_value_max(snapshot, "transcript:sqlite:record_total");
    assert!(
            record_total >= history_items as u64 / 2,
            "display-only resume did not observe total record count: {record_total} for {history_items} history items"
        );
}

fn resume_perf_metric(label: &str) -> bool {
    [
        "transcript:resume_tail",
        "transcript:record_window",
        "store:db",
        "store:transcript",
    ]
    .iter()
    .any(|prefix| label.starts_with(prefix))
}

fn print_resume_perf_snapshot(snapshot: &smelt_perf::perf::Snapshot) {
    for row in snapshot
        .durations
        .iter()
        .filter(|row| resume_perf_metric(row.label))
    {
        eprintln!(
                "TRANSCRIPT_RESUME_PERF_DURATION metric={} count={} last_us={} total_us={} p95_us={} max_us={}",
                row.label, row.count, row.last_us, row.total_us, row.p95_us, row.max_us
            );
        eprintln!(
                "TRANSCRIPT_RESUME_PERF_DURATION_JSON {{\"type\":\"resume_perf_duration\",\"metric\":\"{}\",\"count\":{},\"last_us\":{},\"total_us\":{},\"p95_us\":{},\"max_us\":{}}}",
                row.label, row.count, row.last_us, row.total_us, row.p95_us, row.max_us
            );
    }
    for row in snapshot
        .values
        .iter()
        .filter(|row| resume_perf_metric(row.label))
    {
        eprintln!(
            "TRANSCRIPT_RESUME_PERF_VALUE metric={} count={} last={} total={} p95={} max={}",
            row.label, row.count, row.last, row.total, row.p95, row.max
        );
        eprintln!(
                "TRANSCRIPT_RESUME_PERF_VALUE_JSON {{\"type\":\"resume_perf_value\",\"metric\":\"{}\",\"count\":{},\"last\":{},\"total\":{},\"p95\":{},\"max\":{}}}",
                row.label, row.count, row.last, row.total, row.p95, row.max
            );
    }
}

fn print_layout_alloc_snapshot(workload: &str, snapshot: &smelt_perf::perf::Snapshot) {
    for row in snapshot
        .allocs
        .iter()
        .filter(|row| row.label.starts_with("transcript:"))
    {
        eprintln!(
                "TRANSCRIPT_LAYOUT_ALLOC workload={} metric={} count={} allocs_last={} allocs_total={} bytes_last={} bytes_total={} bytes_p95={} bytes_max={}",
                workload,
                row.label,
                row.count,
                row.allocs_last,
                row.allocs_total,
                row.bytes_last,
                row.bytes_total,
                row.bytes_p95,
                row.bytes_max
            );
        eprintln!(
                "TRANSCRIPT_LAYOUT_ALLOC_JSON {{\"type\":\"layout_alloc\",\"workload\":\"{}\",\"metric\":\"{}\",\"count\":{},\"allocs_last\":{},\"allocs_total\":{},\"bytes_last\":{},\"bytes_total\":{},\"bytes_p95\":{},\"bytes_max\":{}}}",
                workload,
                row.label,
                row.count,
                row.allocs_last,
                row.allocs_total,
                row.bytes_last,
                row.bytes_total,
                row.bytes_p95,
                row.bytes_max
            );
    }
}

fn transcript_bench_runs() -> usize {
    std::env::var("SMELT_TRANSCRIPT_BENCH_RUNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|runs| *runs > 0)
        .unwrap_or(3)
}

fn transcript_bench_workloads() -> Vec<TranscriptBenchWorkload> {
    use crate::benchmark_config::{named_list, LayoutKind, LAYOUT_WORKLOADS};
    let filter = std::env::var("SMELT_TRANSCRIPT_BENCH_WORKLOADS").ok();
    let names = filter.as_deref().map(|value| {
        named_list(value, LAYOUT_WORKLOADS.iter().map(|entry| entry.0))
            .expect("SMELT_TRANSCRIPT_BENCH_WORKLOADS")
    });
    LAYOUT_WORKLOADS
        .iter()
        .filter(|entry| names.as_ref().is_none_or(|names| names.contains(&entry.0)))
        .map(|&(name, target_bytes, kind)| TranscriptBenchWorkload {
            name,
            target_bytes,
            build: match kind {
                LayoutKind::Mixed => push_large_mixed_transcript_fixture,
                LayoutKind::Markdown => push_markdown_heavy_transcript_fixture,
                LayoutKind::ToolOutput => push_tool_output_heavy_transcript_fixture,
                LayoutKind::TinyBlocks => push_many_tiny_blocks_transcript_fixture,
                LayoutKind::HugeBlocks => push_few_huge_blocks_transcript_fixture,
            },
        })
        .collect()
}

const RESUME_RECORD_BLOCK_TEXT_BYTES: usize = 4 * 1024;
const RESUME_RECORD_WRITE_CHUNK: usize = 2048;

fn resume_record_content(block_idx: usize) -> String {
    let mut content = format!("# Resume benchmark response {block_idx}\n\n");
    let paragraph = "This record-backed resume fixture stores full transcript text in session storage without building a full in-memory transcript first. It keeps enough markdown and wrapping pressure to exercise tail hydration and rendering.\n";
    while content.len() < RESUME_RECORD_BLOCK_TEXT_BYTES {
        content.push_str(paragraph);
    }
    smelt_buffer::text::grapheme_prefix(&content, RESUME_RECORD_BLOCK_TEXT_BYTES).to_string()
}

fn resume_block_record(block_idx: usize, content: String) -> smelt_store::StoredTranscriptBlock {
    let record = smelt_core::Block::Text {
        content: content.clone().into(),
    };
    let content_hash = record.content_hash();
    let indexed_text = format!("resume benchmark block {block_idx}");
    let mut row = smelt_store::StoredTranscriptBlock {
        block_idx: block_idx as u64,
        history_idx: None,
        kind: record.kind().to_string(),
        tool_call_id: None,
        tool_name: None,
        content_hash: content_hash.to_string(),
        estimated_text_bytes: content.len() as u64,
        summary: Vec::new(),
        indexed_text,
        block_json: serde_json::to_string(&record).expect("serialize resume record"),
        origin_json: None,
        tool_state_json: None,
        tool_render_revision: 0,
    };
    let parsed = smelt_core::TranscriptBlockRecordWithId::try_from(row.clone())
        .expect("parse resume record");
    row.summary = smelt_transcript::transcript_model::encode_record_summary(
        &parsed.record,
        &row.indexed_text,
        row.estimated_text_bytes,
    )
    .expect("encode resume summary");
    row
}

fn write_record_backed_resume_fixture(
    sessions: &smelt_core::session::SessionStorage,
    session: &smelt_core::session::Session,
    target_bytes: usize,
) -> (usize, usize, f64) {
    let setup_start = std::time::Instant::now();
    let mut writer = sessions
        .store()
        .writer(&session.id)
        .expect("open record resume fixture writer");
    let identity = smelt_core::session::store_identity(session);
    let commit =
        smelt_core::session::full_commit(session).expect("prepare record resume fixture state");
    let mut head = writer
        .commit(&identity, &commit)
        .expect("initialize record resume fixture state");
    let target_bytes = target_bytes.max(RESUME_RECORD_BLOCK_TEXT_BYTES);
    let mut generated_bytes = 0usize;
    let mut record_count = 0usize;
    while generated_bytes < target_bytes {
        let mut records = Vec::with_capacity(RESUME_RECORD_WRITE_CHUNK);
        while generated_bytes < target_bytes && records.len() < RESUME_RECORD_WRITE_CHUNK {
            let content = resume_record_content(record_count);
            generated_bytes = generated_bytes.saturating_add(content.len());
            records.push(resume_block_record(record_count, content));
            record_count += 1;
        }
        let chunk = smelt_store::SessionCommit {
            metadata: commit.metadata.clone(),
            transcript: Some(smelt_store::TranscriptRecordSuffix {
                start: head.transcript_record_count.get(),
                records,
            }),
            ..Default::default()
        };
        head = writer
            .commit(&identity, &chunk)
            .expect("write record resume fixture chunk");
    }
    drop(writer);
    let setup_ms = elapsed_ms(setup_start.elapsed());
    (record_count, generated_bytes, setup_ms)
}

fn run_true_resume_bench_sample(target_bytes: usize) {
    smelt_perf::perf::clear();
    smelt_perf::perf::set_enabled(true);
    let lua = crate::lua::LuaRuntime::new();
    let theme = Theme::default();
    let session = smelt_core::session::Session::new(
        std::process::id(),
        std::env::current_dir().unwrap_or_default(),
    );
    let session_id = session.id.clone();
    let storage_dir = tempfile::tempdir().expect("resume benchmark storage");
    let sessions = smelt_core::session::SessionStorage::new(storage_dir.path().join("smelt"));
    let (record_count, generated_bytes, setup_ms) =
        write_record_backed_resume_fixture(&sessions, &session, target_bytes);

    smelt_perf::perf::clear();
    let tail_alloc_before = smelt_perf::alloc::snapshot();
    let tail_load_start = std::time::Instant::now();
    let tail_resumed =
        crate::app::history::load_transcript_tail_from_sqlite_id(&sessions, &session_id, 100, 40)
            .expect("tail-load benchmark transcript records");
    let mut tail_document =
        crate::app::transcript::TranscriptDocument::from_loaded_transcript(tail_resumed);
    let tail_load_ms = elapsed_ms(tail_load_start.elapsed());
    let mut tail_buf = Buffer::new(smelt_edit::BufId(94), Default::default());
    let tail_render_start = std::time::Instant::now();
    // The tail slice carries record profiles; the first frame hydrates the
    // blocks it shows, as the app's hydration worker does.
    let tail_plan = loop {
        match tail_document.plan_viewport_projection_measured(
            &lua,
            100,
            &theme,
            crate::app::transcript::TranscriptViewportProjectionInput {
                fallback_scroll_top: 0,
                follow_tail: true,
                width_changed: false,
                previous_width: None,
            },
            40,
        ) {
            Ok(plan) => break plan,
            Err(error) => assert!(
                tail_document.service_pending_hydration_for_test(),
                "resume benchmark projection hydration: {error:?}"
            ),
        }
    };
    let tail_rows = tail_document
        .project_applied_viewport(&lua, &mut tail_buf, &theme, tail_plan)
        .materialized_rows;
    let tail_render_ms = elapsed_ms(tail_render_start.elapsed());
    let tail_alloc_after = smelt_perf::alloc::snapshot();
    let tail_alloc = smelt_perf::alloc::delta(tail_alloc_before, tail_alloc_after);
    let tail_retained_bytes =
        tail_alloc_after.current_bytes as i64 - tail_alloc_before.current_bytes as i64;
    assert!(tail_rows.total_rows > 0);
    let tail_snapshot = smelt_perf::perf::snapshot();
    print_resume_perf_snapshot(&tail_snapshot);
    assert_resume_tail_perf_gates(&tail_snapshot, record_count);
    assert_eq!(
        perf_value_max(&tail_snapshot, "transcript:sqlite:record_total"),
        record_count as u64,
        "display-only resume did not observe the record-backed fixture size"
    );

    drop(tail_document);
    sessions
        .delete(&session_id)
        .expect("delete resume benchmark session");
    smelt_perf::perf::set_enabled(false);
    eprintln!(
            "TRANSCRIPT_TRUE_RESUME_SAMPLE mode=record_backed target_bytes={} generated_bytes={} records={} rows={} setup_ms={:.3} tail_load_ms={:.3} tail_render_ms={:.3} tail_bytes_allocated={} tail_bytes_deallocated={} tail_current_bytes_before={} tail_current_bytes_after={} tail_retained_bytes={}",
            target_bytes,
            generated_bytes,
            record_count,
            tail_rows.total_rows,
            setup_ms,
            tail_load_ms,
            tail_render_ms,
            tail_alloc.bytes_allocated,
            tail_alloc.bytes_deallocated,
            tail_alloc_before.current_bytes,
            tail_alloc_after.current_bytes,
            tail_retained_bytes,
        );
    eprintln!(
            "TRANSCRIPT_TRUE_RESUME_JSON {{\"type\":\"resume_summary\",\"mode\":\"record_backed\",\"target_bytes\":{},\"generated_bytes\":{},\"records\":{},\"rows\":{},\"setup_ms\":{:.3},\"tail_load_ms\":{:.3},\"tail_render_ms\":{:.3},\"tail_bytes_allocated\":{},\"tail_bytes_deallocated\":{},\"tail_current_bytes_before\":{},\"tail_current_bytes_after\":{},\"tail_retained_bytes\":{}}}",
            target_bytes,
            generated_bytes,
            record_count,
            tail_rows.total_rows,
            setup_ms,
            tail_load_ms,
            tail_render_ms,
            tail_alloc.bytes_allocated,
            tail_alloc.bytes_deallocated,
            tail_alloc_before.current_bytes,
            tail_alloc_after.current_bytes,
            tail_retained_bytes,
        );
}
fn assert_projection_bench_gates(counters: TranscriptProjectionCounters, label: &str) {
    const MAX_MATERIALIZED_ROWS_PER_OPERATION: usize = 80;
    assert!(
        counters.max_range_materialized_rows <= MAX_MATERIALIZED_ROWS_PER_OPERATION,
        "{label} materialized {} rows, expected <= {MAX_MATERIALIZED_ROWS_PER_OPERATION}",
        counters.max_range_materialized_rows,
    );
    assert!(
        counters.max_range_materialized_blocks <= MAX_MATERIALIZED_ROWS_PER_OPERATION,
        "{label} materialized {} blocks, expected <= {MAX_MATERIALIZED_ROWS_PER_OPERATION}",
        counters.max_range_materialized_blocks,
    );
}

fn run_transcript_bench_sample(workload: TranscriptBenchWorkload) -> TranscriptBenchSample {
    smelt_perf::perf::clear();
    smelt_perf::perf::set_enabled(true);
    smelt_perf::alloc::set_enabled(true);

    let lua = test_lua();
    let mut transcript = Transcript::new();
    let generated_bytes = (workload.build)(&mut transcript, workload.target_bytes);
    let input_bytes = approx_history_bytes(&transcript.history);
    let blocks = transcript.history.order.len();
    let theme = Theme::default();

    let mut cold = TranscriptProjection::new();
    let mut cold_buf = Buffer::new(smelt_edit::BufId(77), Default::default());
    let alloc_start = smelt_perf::alloc::snapshot();
    let first_start = std::time::Instant::now();
    let first = project_with_lua(
        &mut cold,
        &lua,
        &mut cold_buf,
        &mut transcript.history,
        100,
        &theme,
        ScrollTarget::visible_tail(),
        40,
    );
    let first_ms = elapsed_ms(first_start.elapsed());
    let first_alloc = smelt_perf::alloc::delta(alloc_start, smelt_perf::alloc::snapshot());
    let first_snapshot = smelt_perf::perf::snapshot();
    print_layout_alloc_snapshot(workload.name, &first_snapshot);
    let first_counters = cold.counters();

    cold.reset_counters();
    let resize_start = std::time::Instant::now();
    let resized = project_with_lua(
        &mut cold,
        &lua,
        &mut cold_buf,
        &mut transcript.history,
        72,
        &theme,
        ScrollTarget::visible_row(first.clamped_scroll),
        40,
    );
    let resize_ms = elapsed_ms(resize_start.elapsed());
    let resize_counters = cold.counters();

    cold.invalidate_theme();
    cold.reset_counters();
    let theme_start = std::time::Instant::now();
    let themed = project_with_lua(
        &mut cold,
        &lua,
        &mut cold_buf,
        &mut transcript.history,
        72,
        &theme,
        ScrollTarget::visible_row(resized.clamped_scroll),
        40,
    );
    let theme_ms = elapsed_ms(theme_start.elapsed());
    let theme_counters = cold.counters();

    cold.reset_counters();
    let scroll_start = std::time::Instant::now();
    let mut scroll_materialized_rows = 0u64;
    let max_scroll = resized.total_rows.saturating_sub(1);
    let step = (resized.total_rows / 12).max(1);
    for i in 0..12u64 {
        let row = i.saturating_mul(step).min(max_scroll);
        let rows = project_with_lua(
            &mut cold,
            &lua,
            &mut cold_buf,
            &mut transcript.history,
            72,
            &theme,
            ScrollTarget::visible_row(row),
            40,
        );
        scroll_materialized_rows = scroll_materialized_rows.saturating_add(rows.materialized_rows);
    }
    let scroll12_ms = elapsed_ms(scroll_start.elapsed());
    let scroll_counters = cold.counters();

    cold.reset_counters();
    let visible_start = std::time::Instant::now();
    let mid = resized.total_rows / 2;
    let visible =
        cold.display_rows_for_range(&lua, &mut transcript.history, 72, &theme, mid..mid + 80);
    let visible_ms = elapsed_ms(visible_start.elapsed());
    let visible_counters = cold.counters();

    let mut copy_probe = TranscriptProjection::new();
    copy_probe.set_inline_options(cold.inline_options().clone());
    let copy_range = find_selectable_copy_range(
        &mut copy_probe,
        &lua,
        &mut transcript.history,
        72,
        &theme,
        mid,
        resized.total_rows,
    )
    .expect("benchmark transcript should contain selectable display text");
    copy_probe.reset_counters();
    let copy_start = std::time::Instant::now();
    let copied = copy_probe.copy_range(&lua, &mut transcript.history, 72, &theme, copy_range);
    let copy_ms = elapsed_ms(copy_start.elapsed());
    let copy_counters = copy_probe.counters();

    let mut no_cache = TranscriptProjection::new();
    let mut no_cache_buf = Buffer::new(smelt_edit::BufId(80), Default::default());
    let no_cache_start = std::time::Instant::now();
    let no_cache_projection = project_with_lua(
        &mut no_cache,
        &lua,
        &mut no_cache_buf,
        &mut transcript.history,
        100,
        &theme,
        ScrollTarget::visible_tail(),
        40,
    );
    let no_cache_ms = elapsed_ms(no_cache_start.elapsed());
    let no_cache_counters = no_cache.counters();

    transcript.push(Block::Text {
        content: "live append benchmark tail\n".repeat(16).into(),
    });
    cold.reset_counters();
    let append_start = std::time::Instant::now();
    let appended = project_with_lua(
        &mut cold,
        &lua,
        &mut cold_buf,
        &mut transcript.history,
        72,
        &theme,
        ScrollTarget::visible_tail(),
        40,
    );
    let append_ms = elapsed_ms(append_start.elapsed());
    let append_counters = cold.counters();
    let appended_rows = appended.total_rows.saturating_sub(resized.total_rows);

    assert!(generated_bytes >= workload.target_bytes);
    assert!(input_bytes > 0);
    assert!(first.total_rows > 0);
    assert!(resized.total_rows > 0);
    assert_eq!(themed.total_rows, resized.total_rows);
    assert!(
        !visible.rows.is_empty(),
        "visible range was empty at mid={mid} total_rows={} counters={visible_counters:?}",
        resized.total_rows
    );
    assert!(
        !copied.kill_ring.is_empty(),
        "copy range was empty at start={:?} end={:?} total_rows={} counters={copy_counters:?}",
        copy_range.start,
        copy_range.end,
        resized.total_rows
    );
    assert_eq!(no_cache_projection.total_rows, first.total_rows);
    assert!(
        appended.materialized_rows > 0,
        "live append repaint produced no materialized rows: {appended:?}"
    );
    assert!(first_counters.layout_cache > 0);
    assert!(first_counters.layout_cache <= blocks);
    assert!(resize_counters.layout_cache <= blocks);
    assert!(resize_counters.exact_height_measured_blocks <= blocks);
    assert!(theme_counters.layout_cache <= blocks);
    assert!(theme_counters.exact_height_measured_blocks <= blocks);
    assert!(scroll_counters.layout_cache <= blocks);
    assert!(scroll_counters.exact_height_measured_blocks <= blocks);
    assert!(visible_counters.layout_cache <= blocks);
    assert!(visible_counters.exact_height_measured_blocks <= blocks);
    assert!(copy_counters.layout_cache <= blocks);
    assert!(copy_counters.exact_height_measured_blocks <= blocks);
    assert!(append_counters.layout_cache <= blocks + 1);
    assert!(append_counters.exact_height_measured_blocks <= blocks + 1);
    assert_eq!(no_cache_counters.layout_cache, first_counters.layout_cache);
    assert_projection_bench_gates(first_counters, "first paint");
    assert_projection_bench_gates(resize_counters, "resize");
    assert_projection_bench_gates(theme_counters, "theme refresh");
    assert_projection_bench_gates(visible_counters, "visible range");
    assert_projection_bench_gates(copy_counters, "copy range");
    assert_projection_bench_gates(append_counters, "live append");
    assert_projection_bench_gates(no_cache_counters, "no-cache first paint");
    assert_projection_bench_gates(scroll_counters, "scroll jump");
    assert!(
        scroll_counters.range_materialized_rows <= 12 * 80,
        "scroll benchmark materialized {} rows over 12 jumps, expected <= 960",
        scroll_counters.range_materialized_rows,
    );

    smelt_perf::alloc::set_enabled(false);
    smelt_perf::perf::set_enabled(false);
    smelt_perf::perf::clear();

    TranscriptBenchSample {
        input_bytes,
        generated_bytes,
        blocks,
        total_rows: first.total_rows,
        first_ms,
        resize_ms,
        theme_ms,
        scroll12_ms,
        visible_ms,
        copy_ms,
        append_ms,
        no_cache_ms,
        allocs: first_alloc.allocs,
        bytes_allocated: first_alloc.bytes_allocated,
        alloc_current_bytes: first_alloc.current_bytes,
        alloc_peak_bytes: first_alloc.peak_bytes,
        visible_rows: visible.rows.len(),
        copied_rows: copy_range
            .end
            .row
            .saturating_sub(copy_range.start.row)
            .saturating_add(1),
        appended_rows,
        scroll_materialized_rows,
        first_counters,
        resize_counters,
        theme_counters,
        scroll_counters,
        visible_counters,
        copy_counters,
        append_counters,
        no_cache_counters,
    }
}

fn print_transcript_bench_summary(
    workload: TranscriptBenchWorkload,
    samples: &[TranscriptBenchSample],
) {
    let first = MetricStats::from(
        &samples
            .iter()
            .map(|sample| sample.first_ms)
            .collect::<Vec<_>>(),
    );
    let resize = MetricStats::from(
        &samples
            .iter()
            .map(|sample| sample.resize_ms)
            .collect::<Vec<_>>(),
    );
    let theme = MetricStats::from(
        &samples
            .iter()
            .map(|sample| sample.theme_ms)
            .collect::<Vec<_>>(),
    );
    let scroll = MetricStats::from(
        &samples
            .iter()
            .map(|sample| sample.scroll12_ms)
            .collect::<Vec<_>>(),
    );
    let visible = MetricStats::from(
        &samples
            .iter()
            .map(|sample| sample.visible_ms)
            .collect::<Vec<_>>(),
    );
    let copy = MetricStats::from(
        &samples
            .iter()
            .map(|sample| sample.copy_ms)
            .collect::<Vec<_>>(),
    );
    let append = MetricStats::from(
        &samples
            .iter()
            .map(|sample| sample.append_ms)
            .collect::<Vec<_>>(),
    );
    let no_cache = MetricStats::from(
        &samples
            .iter()
            .map(|sample| sample.no_cache_ms)
            .collect::<Vec<_>>(),
    );
    let sample = samples[0];
    eprintln!(
            "| {:<18} | {:>8.2} | {:>6} | {:>8} | {:>12} | {:>12} | {:>12} | {:>12} | {:>12} | {:>12} | {:>12} | {:>12} |",
            workload.name,
            sample.input_bytes as f64 / (1024.0 * 1024.0),
            sample.blocks,
            sample.total_rows,
            first.display(),
            resize.display(),
            theme.display(),
            scroll.display(),
            visible.display(),
            copy.display(),
            append.display(),
            no_cache.display(),
        );
    eprintln!(
            "TRANSCRIPT_LAYOUT_BENCH_SUMMARY workload={} runs={} input_bytes={} generated_bytes={} blocks={} rows={} first_mean_ms={:.3} first_stddev_ms={:.3} resize_mean_ms={:.3} resize_stddev_ms={:.3} theme_mean_ms={:.3} theme_stddev_ms={:.3} scroll12_mean_ms={:.3} scroll12_stddev_ms={:.3} visible_mean_ms={:.3} visible_stddev_ms={:.3} copy_mean_ms={:.3} copy_stddev_ms={:.3} append_mean_ms={:.3} append_stddev_ms={:.3} no_cache_mean_ms={:.3} no_cache_stddev_ms={:.3} allocs={} bytes_allocated={} alloc_current_bytes={} alloc_peak_bytes={} visible_rows={} copied_rows={} appended_rows={} scroll_materialized_rows={} first_min_ms={:.3} first_max_ms={:.3}",
            workload.name,
            samples.len(),
            sample.input_bytes,
            sample.generated_bytes,
            sample.blocks,
            sample.total_rows,
            first.mean,
            first.stddev,
            resize.mean,
            resize.stddev,
            theme.mean,
            theme.stddev,
            scroll.mean,
            scroll.stddev,
            visible.mean,
            visible.stddev,
            copy.mean,
            copy.stddev,
            append.mean,
            append.stddev,
            no_cache.mean,
            no_cache.stddev,
            sample.allocs,
            sample.bytes_allocated,
            sample.alloc_current_bytes,
            sample.alloc_peak_bytes,
            sample.visible_rows,
            sample.copied_rows,
            sample.appended_rows,
            sample.scroll_materialized_rows,
            first.min,
            first.max,
        );
    eprintln!(
            "TRANSCRIPT_LAYOUT_BENCH_JSON {{\"type\":\"layout_summary\",\"workload\":\"{}\",\"runs\":{},\"input_bytes\":{},\"generated_bytes\":{},\"blocks\":{},\"rows\":{},\"first_mean_ms\":{:.3},\"first_stddev_ms\":{:.3},\"resize_mean_ms\":{:.3},\"resize_stddev_ms\":{:.3},\"theme_mean_ms\":{:.3},\"theme_stddev_ms\":{:.3},\"scroll12_mean_ms\":{:.3},\"scroll12_stddev_ms\":{:.3},\"visible_mean_ms\":{:.3},\"visible_stddev_ms\":{:.3},\"copy_mean_ms\":{:.3},\"copy_stddev_ms\":{:.3},\"append_mean_ms\":{:.3},\"append_stddev_ms\":{:.3},\"no_cache_mean_ms\":{:.3},\"no_cache_stddev_ms\":{:.3},\"allocs\":{},\"bytes_allocated\":{},\"alloc_current_bytes\":{},\"alloc_peak_bytes\":{},\"visible_rows\":{},\"copied_rows\":{},\"appended_rows\":{},\"scroll_materialized_rows\":{},\"first_min_ms\":{:.3},\"first_max_ms\":{:.3}}}",
            workload.name,
            samples.len(),
            sample.input_bytes,
            sample.generated_bytes,
            sample.blocks,
            sample.total_rows,
            first.mean,
            first.stddev,
            resize.mean,
            resize.stddev,
            theme.mean,
            theme.stddev,
            scroll.mean,
            scroll.stddev,
            visible.mean,
            visible.stddev,
            copy.mean,
            copy.stddev,
            append.mean,
            append.stddev,
            no_cache.mean,
            no_cache.stddev,
            sample.allocs,
            sample.bytes_allocated,
            sample.alloc_current_bytes,
            sample.alloc_peak_bytes,
            sample.visible_rows,
            sample.copied_rows,
            sample.appended_rows,
            sample.scroll_materialized_rows,
            first.min,
            first.max,
        );
    eprintln!(
            "TRANSCRIPT_LAYOUT_COUNTERS_JSON {{\"type\":\"layout_counters\",\"workload\":\"{}\",\"first\":{},\"resize\":{},\"theme\":{},\"scroll12\":{},\"visible\":{},\"copy\":{},\"append\":{},\"no_cache\":{}}}",
            workload.name,
            counters_json(sample.first_counters),
            counters_json(sample.resize_counters),
            counters_json(sample.theme_counters),
            counters_json(sample.scroll_counters),
            counters_json(sample.visible_counters),
            counters_json(sample.copy_counters),
            counters_json(sample.append_counters),
            counters_json(sample.no_cache_counters),
        );
    eprintln!(
            "TRANSCRIPT_LAYOUT_BENCH_COUNTERS workload={} first={:?} resize={:?} theme={:?} scroll12={:?} visible={:?} copy={:?} append={:?} no_cache={:?}",
            workload.name,
            sample.first_counters,
            sample.resize_counters,
            sample.theme_counters,
            sample.scroll_counters,
            sample.visible_counters,
            sample.copy_counters,
            sample.append_counters,
            sample.no_cache_counters,
        );
}

pub fn run_layout_benchmark() {
    let runs = transcript_bench_runs();
    let workloads = transcript_bench_workloads();
    assert!(!workloads.is_empty(), "no benchmark workloads selected");
    eprintln!(
        "TRANSCRIPT_LAYOUT_BENCH runs={runs} workloads={}",
        workloads.len()
    );
    eprintln!(
            "| workload           |      MiB | blocks |     rows |     first ms |    resize ms |     theme ms |   scroll12 ms |   visible ms |      copy ms |    append ms |   nocache ms |"
        );
    eprintln!(
            "|--------------------|----------|--------|----------|--------------|--------------|--------------|--------------|--------------|--------------|--------------|--------------|"
        );
    for workload in workloads {
        let _warmup = run_transcript_bench_sample(workload);
        let mut samples = Vec::with_capacity(runs);
        for run in 0..runs {
            let sample = run_transcript_bench_sample(workload);
            eprintln!(
                    "TRANSCRIPT_LAYOUT_BENCH_SAMPLE workload={} run={} input_bytes={} generated_bytes={} blocks={} rows={} first_ms={:.3} resize_ms={:.3} theme_ms={:.3} scroll12_ms={:.3} visible_ms={:.3} copy_ms={:.3} append_ms={:.3} no_cache_ms={:.3} allocs={} bytes_allocated={} alloc_current_bytes={} alloc_peak_bytes={}",
                    workload.name,
                    run + 1,
                    sample.input_bytes,
                    sample.generated_bytes,
                    sample.blocks,
                    sample.total_rows,
                    sample.first_ms,
                    sample.resize_ms,
                    sample.theme_ms,
                    sample.scroll12_ms,
                    sample.visible_ms,
                    sample.copy_ms,
                    sample.append_ms,
                    sample.no_cache_ms,
                    sample.allocs,
                    sample.bytes_allocated,
                    sample.alloc_current_bytes,
                    sample.alloc_peak_bytes,
                );
            samples.push(sample);
        }
        print_transcript_bench_summary(workload, &samples);
    }
}

pub fn run_true_resume_benchmark() {
    let target_bytes = std::env::var("SMELT_TRANSCRIPT_RESUME_BENCH_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(10 * 1024 * 1024);
    run_true_resume_bench_sample(target_bytes);
}
