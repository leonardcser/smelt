use super::*;

#[derive(Clone, Copy, Debug)]
struct NavSample {
    search_ms: f64,
    ctrl_d20_ms: f64,
    ctrl_u20_ms: f64,
    gg_ms: f64,
    g_ms: f64,
    rows: crate::smelt_edit::RowIndex,
}

#[derive(Clone, Copy, Debug)]
struct NavStats {
    mean: f64,
    stddev: f64,
}

impl NavStats {
    fn from(values: &[f64]) -> Self {
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let variance = if values.len() > 1 {
            values
                .iter()
                .map(|value| {
                    let delta = value - mean;
                    delta * delta
                })
                .sum::<f64>()
                / (values.len() - 1) as f64
        } else {
            0.0
        };
        Self {
            mean,
            stddev: variance.sqrt(),
        }
    }

    fn display(self) -> String {
        format!("{:.2}±{:.2}", self.mean, self.stddev)
    }
}

fn elapsed_ms(elapsed: std::time::Duration) -> f64 {
    elapsed.as_secs_f64() * 1_000.0
}

fn env_positive_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn navigation_bench_runs() -> usize {
    env_positive_usize("SMELT_TRANSCRIPT_BENCH_RUNS", 3)
}

fn transcript_navigation_bench_app() -> TestApp {
    let mut app = TestApp::builder().with_vim(true).build();
    app.app.handle_resize(100, 32);
    for i in 0..8_000 {
        let marker = if i == 7_777 { " needle-target" } else { "" };
        app.app
            .push_block(smelt_core::transcript_model::Block::Text {
                content: format!(
                    "navigation bench row {i:04}{marker}: {}",
                    "alpha beta gamma delta ".repeat(3)
                ),
            });
    }
    app.render_silent();
    app.app.app_focus = AppFocus::Content;
    app.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
    let win = app.app.transcript_win_mut();
    win.set_vim_enabled(true);
    win.set_vim_mode(VimMode::Normal);
    app
}

fn run_navigation_sample() -> NavSample {
    let mut app = transcript_navigation_bench_app();
    let rows = transcript_total_rows(&app);

    app.type_char('g');
    app.type_char('g');
    let search_start = std::time::Instant::now();
    app.type_char('/');
    app.type_text("needle-target");
    app.press(KeyCode::Enter);
    app.render_silent();
    let search_ms = elapsed_ms(search_start.elapsed());
    assert!(app.app.search.session.is_some());
    assert!(transcript_row_cursor_row(&app) > rows / 2);

    app.type_char('g');
    app.type_char('g');
    app.render_silent();
    let ctrl_d_start = std::time::Instant::now();
    for _ in 0..20 {
        app.press_mod(KeyCode::Char('d'), KeyModifiers::CONTROL);
        app.render_silent();
    }
    let ctrl_d20_ms = elapsed_ms(ctrl_d_start.elapsed());
    assert!(transcript_row_cursor_row(&app) > 0);

    app.type_char('G');
    app.render_silent();
    let ctrl_u_start = std::time::Instant::now();
    for _ in 0..20 {
        app.press_mod(KeyCode::Char('u'), KeyModifiers::CONTROL);
        app.render_silent();
    }
    let ctrl_u20_ms = elapsed_ms(ctrl_u_start.elapsed());
    assert!(transcript_row_cursor_row(&app) < rows.saturating_sub(1));

    let gg_start = std::time::Instant::now();
    app.type_char('g');
    app.type_char('g');
    app.render_silent();
    let gg_ms = elapsed_ms(gg_start.elapsed());
    assert_eq!(transcript_row_cursor_row(&app), 0);

    let g_start = std::time::Instant::now();
    app.type_char('G');
    app.render_silent();
    let g_ms = elapsed_ms(g_start.elapsed());
    assert!(transcript_row_cursor_row(&app) >= rows.saturating_sub(2));

    NavSample {
        search_ms,
        ctrl_d20_ms,
        ctrl_u20_ms,
        gg_ms,
        g_ms,
        rows,
    }
}

fn search_bench_bytes() -> usize {
    env_positive_usize("SMELT_TRANSCRIPT_BENCH_SEARCH_BYTES", 50 * 1024 * 1024)
}

fn transcript_bench_warmup_enabled() -> bool {
    !matches!(
        std::env::var("SMELT_TRANSCRIPT_BENCH_NO_WARMUP").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn push_search_bench_transcript(app: &mut TestApp, target_bytes: usize) -> usize {
    let mut approx_bytes = 0usize;
    let mut i = 0usize;
    while approx_bytes < target_bytes {
        let rare = if approx_bytes > target_bytes * 9 / 10 && i.is_multiple_of(17) {
            " needle-target"
        } else {
            ""
        };
        let user = format!(
            "search bench prompt {i}{rare}: {}",
            "common-token transcript search cached navigation ".repeat(10)
        );
        approx_bytes += user.len();
        app.app
            .push_block(smelt_core::transcript_model::Block::User {
                text: user,
                image_labels: vec![],
                command: false,
            });

        let assistant = format!(
            "# Search batch {i}\n\n{}\n\n```rust\nfn search_bench_{i}() {{ println!(\"common-token {i}\"); }}\n```\n\n{}",
            "markdown rendered rows include common-token and wrapping pressure ".repeat(22),
            "tail rows remain searchable through the transcript trigram index ".repeat(18),
        );
        approx_bytes += assistant.len();
        app.app
            .push_block(smelt_core::transcript_model::Block::Text { content: assistant });

        if i.is_multiple_of(9) {
            let command = format!("python bench_search.py --batch {i}");
            let output = (0..60)
                .map(|j| {
                    format!(
                        "tool result {i}.{j}: {}",
                        "common-token visible materialization exact verification ".repeat(6)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            approx_bytes += command.len() + output.len();
            app.app
                .push_block(smelt_core::transcript_model::Block::Exec { command, output });
        }
        i += 1;
    }
    approx_bytes
}

fn transcript_perf_metric(label: &str) -> bool {
    ["search:transcript", "session:", "store:", "transcript:"]
        .iter()
        .any(|prefix| label.starts_with(prefix))
}

fn search_perf_snapshot(label: &str, snapshot: &smelt_perf::perf::Snapshot) {
    for row in snapshot
        .durations
        .iter()
        .filter(|row| transcript_perf_metric(row.label))
    {
        eprintln!(
            "TRANSCRIPT_SEARCH_PERF_DURATION label={} metric={} count={} last_us={} total_us={} p95_us={} max_us={}",
            label, row.label, row.count, row.last_us, row.total_us, row.p95_us, row.max_us
        );
    }
    for row in snapshot
        .values
        .iter()
        .filter(|row| transcript_perf_metric(row.label))
    {
        eprintln!(
            "TRANSCRIPT_SEARCH_PERF_VALUE label={} metric={} count={} last={} total={} p95={} max={}",
            label, row.label, row.count, row.last, row.total, row.p95, row.max
        );
    }
}

fn perf_value_max(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> u64 {
    snapshot
        .values
        .iter()
        .find(|row| row.label == label)
        .map(|row| row.max)
        .unwrap_or(0)
}

fn perf_duration_max(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> u64 {
    snapshot
        .durations
        .iter()
        .find(|row| row.label == label)
        .map(|row| row.max_us)
        .unwrap_or(0)
}

fn perf_duration_count(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> usize {
    snapshot
        .durations
        .iter()
        .find(|row| row.label == label)
        .map(|row| row.count)
        .unwrap_or(0)
}

fn perf_value_total(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> u64 {
    snapshot
        .values
        .iter()
        .find(|row| row.label == label)
        .map(|row| row.total)
        .unwrap_or(0)
}

fn assert_no_full_search_hot_path_reads(snapshot: &smelt_perf::perf::Snapshot, label: &str) {
    for metric in [
        "store:history:read_all",
        "store:history:read_all_rows",
        "store:session:load_full_snapshot",
        "store:session:full_snapshot_rows_read",
        "store:transcript:read_descriptors_full",
        "store:transcript:descriptors_full_loaded",
        "transcript:build_from_session:history_items",
    ] {
        let value = perf_value_max(snapshot, metric);
        assert_eq!(
            value, 0,
            "{label} recorded {metric}={value}, expected no full-session search work"
        );
    }
}

fn assert_search_refinement_gates(
    snapshot: &smelt_perf::perf::Snapshot,
    label: &str,
    max_rows_per_refinement: u64,
    max_total_rows: u64,
) {
    assert_no_full_search_hot_path_reads(snapshot, label);
    for metric in [
        "search:transcript:scanned_rows",
        "transcript:display_rows_for_range:rows",
        "transcript:exactified_rows",
    ] {
        let max = perf_value_max(snapshot, metric);
        let total = perf_value_total(snapshot, metric);
        assert!(
            max <= max_rows_per_refinement,
            "{label} recorded {metric} max {max}, expected <= {max_rows_per_refinement}"
        );
        assert!(
            total <= max_total_rows,
            "{label} recorded {metric} total {total}, expected <= {max_total_rows}"
        );
    }
}

fn assert_search_uses_candidate_index(
    snapshot: &smelt_perf::perf::Snapshot,
    label: &str,
    max_index_entries: u64,
) {
    let sqlite_available = perf_value_max(snapshot, "search:transcript:sqlite_available");
    assert_eq!(
        sqlite_available, 1,
        "{label} did not use the persisted transcript search index"
    );
    let trigram_build = perf_value_max(snapshot, "search:transcript:index_trigram_build_enabled");
    assert_eq!(
        trigram_build, 0,
        "{label} rebuilt the full transcript trigram index"
    );
    let entries = perf_value_max(snapshot, "search:transcript:index_entries");
    assert!(
        entries <= max_index_entries,
        "{label} indexed {entries} candidate entries, expected <= {max_index_entries}"
    );
}

#[derive(Clone, Copy, Debug)]
struct SearchBenchSample {
    bytes: usize,
    rows: crate::smelt_edit::RowIndex,
    width_resize_ms: f64,
    height_resize_ms: f64,
    theme_color_ms: f64,
    copy_mid_ms: f64,
    nav_ctrl_d20_ms: f64,
    nav_ctrl_u20_ms: f64,
    nav_gg_ms: f64,
    nav_g_ms: f64,
    prod_burst_top_ctrl_d80_ms: f64,
    prod_burst_top_down80_ms: f64,
    prod_burst_bottom_ctrl_u80_ms: f64,
    prod_burst_bottom_up80_ms: f64,
    prod_burst_mid_ctrl_d80_ms: f64,
    prod_burst_mid_down80_ms: f64,
    prod_burst_mid_ctrl_u80_ms: f64,
    prod_burst_mid_up80_ms: f64,
    burst_top_ctrl_d80_ms: f64,
    burst_top_down80_ms: f64,
    burst_bottom_ctrl_u80_ms: f64,
    burst_bottom_up80_ms: f64,
    burst_mid_ctrl_d80_ms: f64,
    burst_mid_down80_ms: f64,
    burst_mid_ctrl_u80_ms: f64,
    burst_mid_up80_ms: f64,
    rare_ms: f64,
    short_common_ms: f64,
    short_absent_ms: f64,
    common_submit_ms: f64,
    next100_ms: f64,
    sparse_rare_ms: f64,
    sparse_common_submit_ms: f64,
    sparse_next100_ms: f64,
    after_append_ms: f64,
}

fn search_bench_metric_values(sample: &SearchBenchSample) -> [(&'static str, f64); 33] {
    [
        ("width_resize", sample.width_resize_ms),
        ("height_resize", sample.height_resize_ms),
        ("theme_color", sample.theme_color_ms),
        ("copy_mid", sample.copy_mid_ms),
        ("nav_ctrl_d20", sample.nav_ctrl_d20_ms),
        ("nav_ctrl_u20", sample.nav_ctrl_u20_ms),
        ("nav_gg", sample.nav_gg_ms),
        ("nav_G", sample.nav_g_ms),
        ("prod_burst_top_ctrl_d80", sample.prod_burst_top_ctrl_d80_ms),
        ("prod_burst_top_down80", sample.prod_burst_top_down80_ms),
        (
            "prod_burst_bottom_ctrl_u80",
            sample.prod_burst_bottom_ctrl_u80_ms,
        ),
        ("prod_burst_bottom_up80", sample.prod_burst_bottom_up80_ms),
        ("prod_burst_mid_ctrl_d80", sample.prod_burst_mid_ctrl_d80_ms),
        ("prod_burst_mid_down80", sample.prod_burst_mid_down80_ms),
        ("prod_burst_mid_ctrl_u80", sample.prod_burst_mid_ctrl_u80_ms),
        ("prod_burst_mid_up80", sample.prod_burst_mid_up80_ms),
        ("burst_top_ctrl_d80", sample.burst_top_ctrl_d80_ms),
        ("burst_top_down80", sample.burst_top_down80_ms),
        ("burst_bottom_ctrl_u80", sample.burst_bottom_ctrl_u80_ms),
        ("burst_bottom_up80", sample.burst_bottom_up80_ms),
        ("burst_mid_ctrl_d80", sample.burst_mid_ctrl_d80_ms),
        ("burst_mid_down80", sample.burst_mid_down80_ms),
        ("burst_mid_ctrl_u80", sample.burst_mid_ctrl_u80_ms),
        ("burst_mid_up80", sample.burst_mid_up80_ms),
        ("rare", sample.rare_ms),
        ("short_common", sample.short_common_ms),
        ("short_absent", sample.short_absent_ms),
        ("common_submit", sample.common_submit_ms),
        ("next100", sample.next100_ms),
        ("sparse_rare", sample.sparse_rare_ms),
        ("sparse_common_submit", sample.sparse_common_submit_ms),
        ("sparse_next100", sample.sparse_next100_ms),
        ("after_append", sample.after_append_ms),
    ]
}

fn assert_no_full_block_renders_for_scroll(snapshot: &smelt_perf::perf::Snapshot, label: &str) {
    for metric in [
        "transcript:display_model:render_full_to_buffer",
        "transcript:display_model:render_full_fallback",
    ] {
        let full_renders = perf_duration_count(snapshot, metric);
        assert_eq!(
            full_renders, 0,
            "{label} recorded {metric} {full_renders} times while scrolling; row anchors must stay lightweight"
        );
    }
}

fn assert_view_operation_gates(snapshot: &smelt_perf::perf::Snapshot, label: &str) {
    assert_no_full_search_hot_path_reads(snapshot, label);
    let materialized_rows = perf_value_total(snapshot, "transcript:collect_nodes_range:rows");
    assert!(
        materialized_rows <= 1024,
        "{label} materialized {materialized_rows} rows, expected bounded viewport work"
    );
    let prepared = perf_value_max(snapshot, "transcript:prepare_row_index:reused_index");
    assert_eq!(
        prepared, 1,
        "{label} rebuilt the row index instead of reusing it"
    );
    let rebuild_us = perf_duration_max(snapshot, "transcript:prepare_row_index:rebuild_index");
    assert_eq!(
        rebuild_us, 0,
        "{label} rebuilt the full row index in {rebuild_us}us"
    );
    if matches!(label, "nav_ctrl_d20" | "nav_ctrl_u20") {
        assert_no_full_block_renders_for_scroll(snapshot, label);
    }
}

fn assert_copy_operation_gates(
    snapshot: &smelt_perf::perf::Snapshot,
    label: &str,
    max_requested_rows: u64,
) {
    assert_no_full_search_hot_path_reads(snapshot, label);
    let exactified_rows = perf_value_total(snapshot, "transcript:exactified_rows");
    assert!(
        exactified_rows <= max_requested_rows,
        "{label} exactified {exactified_rows} rows, expected <= {max_requested_rows}"
    );
    let materialized_rows = perf_value_total(snapshot, "transcript:collect_nodes_range:rows");
    assert!(
        materialized_rows <= 256,
        "{label} materialized {materialized_rows} rows, expected bounded copy work"
    );
    let prepared = perf_value_max(snapshot, "transcript:prepare_row_index:reused_index");
    assert_eq!(
        prepared, 1,
        "{label} rebuilt the row index instead of reusing it"
    );
    let rebuild_us = perf_duration_max(snapshot, "transcript:prepare_row_index:rebuild_index");
    assert_eq!(
        rebuild_us, 0,
        "{label} rebuilt the full row index in {rebuild_us}us"
    );
}

fn assert_burst_operation_gates(snapshot: &smelt_perf::perf::Snapshot, label: &str) {
    assert_no_full_search_hot_path_reads(snapshot, label);
    let materialized_rows = perf_value_total(snapshot, "transcript:collect_nodes_range:rows");
    assert!(
        materialized_rows <= 4096,
        "{label} materialized {materialized_rows} rows, expected bounded viewport work"
    );
    assert_no_full_block_renders_for_scroll(snapshot, label);
}

#[derive(Clone, Copy, Debug)]
enum BurstBenchKey {
    CtrlD,
    CtrlU,
    Down,
    Up,
}

impl BurstBenchKey {
    fn press(self, app: &mut TestApp) {
        match self {
            Self::CtrlD => app.press_mod(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Self::CtrlU => app.press_mod(KeyCode::Char('u'), KeyModifiers::CONTROL),
            Self::Down => app.press(KeyCode::Down),
            Self::Up => app.press(KeyCode::Up),
        }
    }

    fn max_rows_per_event(self, viewport_rows: u16) -> crate::smelt_edit::RowIndex {
        match self {
            Self::CtrlD | Self::CtrlU => crate::smelt_edit::RowIndex::from(viewport_rows.max(1)),
            Self::Down | Self::Up => 1,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum BurstBenchPosition {
    Top,
    Middle,
    Bottom,
}

fn save_bench_fixture(app: &mut TestApp, label: &str) {
    app.app.save_session();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let outcome = app.app.flush_persist();
        match outcome {
            crate::persist::PersistenceFlushOutcome::Durable {
                receipt: Some(_), ..
            } => break,
            crate::persist::PersistenceFlushOutcome::Deadline { .. }
                if std::time::Instant::now() < deadline => {}
            other => panic!("{label} fixture persistence failed: {other:?}"),
        }
    }
}

fn install_sparse_resume_bench_transcript(app: &mut TestApp) {
    let session_dir = smelt_core::session::dir_for(&app.app.core.session);
    let loaded = crate::app::history::load_transcript_tail_from_sqlite_dir(session_dir, 100, 32)
        .expect("load sparse bench transcript tail");
    app.app.clear_transcript();
    app.app
        .session_document
        .transcript
        .replace_loaded_transcript(loaded);
    app.app.handle_resize(100, 32);
    app.app.app_focus = AppFocus::Content;
    app.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
    let win = app.app.transcript_win_mut();
    win.set_vim_enabled(true);
    win.set_vim_mode(VimMode::Normal);
    app.render_silent();
}

fn prepare_burst_bench_position(app: &mut TestApp, position: BurstBenchPosition) {
    match position {
        BurstBenchPosition::Top => {
            app.type_char('g');
            app.type_char('g');
        }
        BurstBenchPosition::Middle => {
            let descriptor = app
                .app
                .session_document
                .transcript
                .descriptor_total_count()
                .expect("sparse descriptor count")
                / 2;
            assert!(
                app.app
                    .reveal_transcript_descriptor_block(descriptor, 1, true),
                "middle descriptor reveal failed for descriptor {descriptor}"
            );
        }
        BurstBenchPosition::Bottom => {
            app.type_char('G');
        }
    }
    app.render_silent();
}

fn assert_burst_bench_projection_bounded(
    label: &str,
    frames: &[crate::app::transcript_scroll_trace::TranscriptScrollTraceFrame],
    before_scroll: crate::smelt_edit::RowIndex,
    max_input_rows: crate::smelt_edit::RowIndex,
    max_projection_delta: crate::smelt_edit::RowIndex,
    strict_projection_delta: bool,
) {
    let mut checked = 0usize;
    for frame in frames {
        let crate::app::transcript_scroll_trace::TranscriptScrollIntent::UserDelta { rows } =
            frame.scroll_intent
        else {
            continue;
        };
        let input_rows = rows.unsigned_abs() as crate::smelt_edit::RowIndex;
        assert!(
            input_rows <= max_input_rows,
            "{label} over-accumulated a single burst input: input_rows={input_rows}, max_input_rows={max_input_rows}, frame={frame:?}"
        );
        let crate::app::transcript_scroll_trace::TranscriptProjectionTargetTrace::ExactRow(target) =
            frame.projection_target
        else {
            panic!("{label} projected user delta through a non-exact target: {frame:?}");
        };
        if strict_projection_delta {
            let delta = target.abs_diff(before_scroll);
            assert!(
                delta <= max_projection_delta,
                "{label} over-accumulated burst navigation: before_scroll={before_scroll}, target={target}, delta={delta}, max_delta={max_projection_delta}, frame={frame:?}"
            );
        }
        checked += 1;
    }
    assert!(
        checked > 0,
        "{label} produced no user-delta projection frames: {frames:?}"
    );
}

fn measure_transcript_burst_operation(
    app: &mut TestApp,
    label: &'static str,
    report_perf: bool,
    position: BurstBenchPosition,
    key: BurstBenchKey,
) -> f64 {
    const BURST_EVENTS: usize = 80;

    prepare_burst_bench_position(app, position);
    let viewport_rows = app
        .app
        .transcript_win()
        .viewport
        .expect("transcript viewport")
        .rect
        .height
        .max(1);
    let before_scroll = app.app.transcript_win().scroll_top();
    app.app
        .session_document
        .transcript
        .set_scroll_trace_timings_enabled(true);
    app.app
        .session_document
        .transcript
        .take_scroll_trace_frames();
    smelt_perf::perf::clear();
    let start = std::time::Instant::now();
    for _ in 0..BURST_EVENTS {
        key.press(app);
    }
    assert_eq!(
        app.app.transcript_win().scroll_top(),
        before_scroll,
        "{label} mutated Window::scroll_top before projection"
    );
    app.render_silent();
    let ms = elapsed_ms(start.elapsed());
    let frames = app
        .app
        .session_document
        .transcript
        .take_scroll_trace_frames();
    let max_input_rows = key.max_rows_per_event(viewport_rows);
    let max_projection_delta = max_input_rows
        .saturating_mul(BURST_EVENTS as crate::smelt_edit::RowIndex)
        .saturating_add(crate::smelt_edit::RowIndex::from(viewport_rows).saturating_mul(2));
    assert_burst_bench_projection_bounded(
        label,
        &frames,
        before_scroll,
        max_input_rows,
        max_projection_delta,
        matches!(position, BurstBenchPosition::Top),
    );
    let snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot(label, &snapshot);
    }
    assert_burst_operation_gates(&snapshot, label);
    ms
}

fn measure_transcript_burst_operation_without_trace(
    app: &mut TestApp,
    label: &'static str,
    report_perf: bool,
    position: BurstBenchPosition,
    key: BurstBenchKey,
) -> f64 {
    const BURST_EVENTS: usize = 80;

    prepare_burst_bench_position(app, position);
    let before_scroll = app.app.transcript_win().scroll_top();
    app.app
        .session_document
        .transcript
        .set_scroll_trace_enabled(false);
    smelt_perf::perf::clear();
    let start = std::time::Instant::now();
    for _ in 0..BURST_EVENTS {
        key.press(app);
    }
    assert_eq!(
        app.app.transcript_win().scroll_top(),
        before_scroll,
        "{label} mutated Window::scroll_top before projection"
    );
    app.render_silent();
    let ms = elapsed_ms(start.elapsed());
    let snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot(label, &snapshot);
    }
    assert_burst_operation_gates(&snapshot, label);
    ms
}

fn measure_transcript_view_operation(
    app: &mut TestApp,
    label: &'static str,
    report_perf: bool,
    operation: impl FnOnce(&mut TestApp),
) -> f64 {
    smelt_perf::perf::clear();
    let start = std::time::Instant::now();
    operation(app);
    app.render_silent();
    let ms = elapsed_ms(start.elapsed());
    let snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot(label, &snapshot);
    }
    assert_view_operation_gates(&snapshot, label);
    ms
}

fn measure_transcript_copy_operation(
    app: &mut TestApp,
    label: &'static str,
    report_perf: bool,
    start_row: crate::smelt_edit::RowIndex,
    copied_rows: crate::smelt_edit::RowIndex,
) -> f64 {
    let end_row = start_row.saturating_add(copied_rows.saturating_sub(1));
    let range = crate::smelt_edit::DocRange {
        start: crate::smelt_edit::DocPosition {
            row: start_row,
            byte_col: 0,
        },
        end: crate::smelt_edit::DocPosition {
            row: end_row,
            byte_col: usize::MAX,
        },
    };
    smelt_perf::perf::clear();
    let start = std::time::Instant::now();
    let out = app
        .app
        .copy_document_rows(crate::app::TRANSCRIPT_WIN, range)
        .expect("copy transcript rows");
    let ms = elapsed_ms(start.elapsed());
    assert!(!out.clipboard.is_empty());
    assert!(!out.kill_ring.is_empty());
    let snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot(label, &snapshot);
    }
    assert_copy_operation_gates(&snapshot, label, copied_rows.saturating_add(1));
    ms
}

fn measure_sparse_search_navigation(app: &mut TestApp, report_perf: bool) -> (f64, f64, f64) {
    app.app
        .session_document
        .transcript
        .set_scroll_trace_enabled(false);
    smelt_perf::perf::clear();
    let rare_start = std::time::Instant::now();
    app.app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        crate::app::search::SearchDirection::Forward,
        "needle-target".into(),
    );
    app.render_silent();
    let rare_ms = elapsed_ms(rare_start.elapsed());
    let rare_snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot("sparse_rare_cold", &rare_snapshot);
    }
    assert_no_full_search_hot_path_reads(&rare_snapshot, "sparse_rare_cold");

    smelt_perf::perf::clear();
    let common_start = std::time::Instant::now();
    app.app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        crate::app::search::SearchDirection::Forward,
        "common-token".into(),
    );
    app.render_silent();
    let common_submit_ms = elapsed_ms(common_start.elapsed());

    let next_start = std::time::Instant::now();
    for _ in 0..100 {
        app.type_char('n');
        app.render_silent();
    }
    let next100_ms = elapsed_ms(next_start.elapsed());
    let common_snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot("sparse_common_hot_next100", &common_snapshot);
    }
    assert_no_full_search_hot_path_reads(&common_snapshot, "sparse_common_hot_next100");
    (rare_ms, common_submit_ms, next100_ms)
}

fn resumed_wheel_bench_bytes() -> usize {
    env_positive_usize("SMELT_TRANSCRIPT_RESUMED_WHEEL_BYTES", 5 * 1024 * 1024)
}

fn resumed_wheel_bench_frames() -> usize {
    env_positive_usize("SMELT_TRANSCRIPT_RESUMED_WHEEL_FRAMES", 240)
}

fn resumed_wheel_bench_ticks_per_frame() -> usize {
    env_positive_usize("SMELT_TRANSCRIPT_RESUMED_WHEEL_TICKS", 24)
}

fn run_resumed_wheel_scroll_bench_sample(
    target_bytes: usize,
    frames: usize,
    ticks_per_frame: usize,
) {
    let mut app = TestApp::builder().with_vim(true).build();
    app.app.handle_resize(100, 32);
    let bytes = push_search_bench_transcript(&mut app, target_bytes);
    app.render_silent();
    save_bench_fixture(&mut app, "resumed wheel");

    install_sparse_resume_bench_transcript(&mut app);
    app.type_char('G');
    app.render_silent();
    app.app
        .session_document
        .transcript
        .set_scroll_trace_timings_enabled(true);
    app.app
        .session_document
        .transcript
        .take_scroll_trace_frames();

    smelt_perf::perf::clear();
    smelt_perf::perf::set_enabled(true);
    let start = std::time::Instant::now();
    for _ in 0..frames {
        for _ in 0..ticks_per_frame {
            app.transcript_scroll_probe_wheel(false, 1);
        }
        app.render_silent();
    }
    let ms = elapsed_ms(start.elapsed());
    let snapshot = smelt_perf::perf::snapshot();
    smelt_perf::perf::set_enabled(false);
    let trace_frames = app
        .app
        .session_document
        .transcript
        .take_scroll_trace_frames();
    let descriptor_loads = perf_duration_count(&snapshot, "store:transcript:read_descriptor_slice");
    let descriptor_load_us = duration_total_us(&snapshot, "store:transcript:read_descriptor_slice");
    let row_rebuilds = perf_duration_count(&snapshot, "transcript:prepare_row_index:rebuild_index");
    let row_rebuild_us = duration_total_us(&snapshot, "transcript:prepare_row_index:rebuild_index");
    search_perf_snapshot("resumed_wheel_scroll", &snapshot);
    eprintln!(
        "TRANSCRIPT_RESUMED_WHEEL_SAMPLE bytes={} frames={} ticks_per_frame={} trace_frames={} wall_ms={:.3} descriptor_loads={} descriptor_load_ms={:.3} row_rebuilds={} row_rebuild_ms={:.3}",
        bytes,
        frames,
        ticks_per_frame,
        trace_frames.len(),
        ms,
        descriptor_loads,
        descriptor_load_us as f64 / 1000.0,
        row_rebuilds,
        row_rebuild_us as f64 / 1000.0,
    );
    eprintln!(
        "TRANSCRIPT_RESUMED_WHEEL_JSON {{\"type\":\"resumed_wheel_summary\",\"bytes\":{},\"frames\":{},\"ticks_per_frame\":{},\"trace_frames\":{},\"wall_ms\":{:.3},\"descriptor_loads\":{},\"descriptor_load_ms\":{:.3},\"row_rebuilds\":{},\"row_rebuild_ms\":{:.3}}}",
        bytes,
        frames,
        ticks_per_frame,
        trace_frames.len(),
        ms,
        descriptor_loads,
        descriptor_load_us as f64 / 1000.0,
        row_rebuilds,
        row_rebuild_us as f64 / 1000.0,
    );
}

#[test]
#[ignore = "manual sparse resumed-session wheel-scroll benchmark; run via `cargo xtask bench-transcript-layout --resumed-wheel`"]
fn transcript_resumed_wheel_scroll_benchmark_suite() {
    run_resumed_wheel_scroll_bench_sample(
        resumed_wheel_bench_bytes(),
        resumed_wheel_bench_frames(),
        resumed_wheel_bench_ticks_per_frame(),
    );
}

fn run_search_bench_sample(target_bytes: usize, report_perf: bool) -> SearchBenchSample {
    smelt_perf::perf::clear();
    smelt_perf::perf::set_enabled(true);
    let mut app = TestApp::builder().with_vim(true).build();
    app.app.handle_resize(100, 32);
    let bytes = push_search_bench_transcript(&mut app, target_bytes);
    app.render_silent();
    save_bench_fixture(&mut app, "search");
    smelt_perf::perf::clear();
    app.app.app_focus = AppFocus::Content;
    app.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
    let win = app.app.transcript_win_mut();
    win.set_vim_enabled(true);
    win.set_vim_mode(VimMode::Normal);
    let rows = transcript_total_rows(&app);

    let width_resize_ms =
        measure_transcript_view_operation(&mut app, "resize_width", report_perf, |app| {
            app.app.handle_resize(140, 32)
        });
    let height_resize_ms =
        measure_transcript_view_operation(&mut app, "resize_height", report_perf, |app| {
            app.app.handle_resize(140, 48)
        });
    let theme_color_ms =
        measure_transcript_view_operation(&mut app, "theme_color", report_perf, |app| {
            app.app.mutate_theme(|theme| {
                theme.set(
                    "SmeltAccent",
                    smelt_core::style::Style::new().fg(smelt_core::style::Color::Rgb {
                        r: 190,
                        g: 120,
                        b: 255,
                    }),
                );
            });
        });
    app.app.handle_resize(100, 32);
    app.render_silent();

    let copy_start_row = rows.saturating_mul(2) / 3;
    let copy_mid_ms = measure_transcript_copy_operation(
        &mut app,
        "copy_mid_rows",
        report_perf,
        copy_start_row,
        8,
    );

    app.type_char('g');
    app.type_char('g');
    app.render_silent();
    let nav_ctrl_d20_ms =
        measure_transcript_view_operation(&mut app, "nav_ctrl_d20", report_perf, |app| {
            for _ in 0..20 {
                app.press_mod(KeyCode::Char('d'), KeyModifiers::CONTROL);
                app.render_silent();
            }
        });
    assert!(transcript_row_cursor_row(&app) > 0);

    app.type_char('G');
    app.render_silent();
    let nav_ctrl_u20_ms =
        measure_transcript_view_operation(&mut app, "nav_ctrl_u20", report_perf, |app| {
            for _ in 0..20 {
                app.press_mod(KeyCode::Char('u'), KeyModifiers::CONTROL);
                app.render_silent();
            }
        });
    assert!(transcript_row_cursor_row(&app) < rows.saturating_sub(1));

    let nav_gg_ms = measure_transcript_view_operation(&mut app, "nav_gg", report_perf, |app| {
        app.type_char('g');
        app.type_char('g');
    });
    assert_eq!(transcript_row_cursor_row(&app), 0);

    let nav_g_ms = measure_transcript_view_operation(&mut app, "nav_G", report_perf, |app| {
        app.type_char('G');
    });
    let row = transcript_row_cursor_row(&app);
    let total_rows_after_g = app.app.transcript_total_rows();
    assert_eq!(
        row,
        total_rows_after_g.saturating_sub(1),
        "G should land on the final transcript row"
    );

    app.type_char('g');
    app.type_char('g');
    app.render_silent();

    smelt_perf::perf::clear();
    let rare_start = std::time::Instant::now();
    app.app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        crate::app::search::SearchDirection::Forward,
        "needle-target".into(),
    );
    app.render_silent();
    let rare_ms = elapsed_ms(rare_start.elapsed());
    assert!(transcript_row_cursor_row(&app) > rows / 2);
    let rare_snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot("rare_cold", &rare_snapshot);
    }
    assert_search_refinement_gates(&rare_snapshot, "rare_cold", 80, 1024);
    assert_search_uses_candidate_index(&rare_snapshot, "rare_cold", 1024);

    app.type_char('g');
    app.type_char('g');
    app.render_silent();
    smelt_perf::perf::clear();
    let short_common_start = std::time::Instant::now();
    app.app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        crate::app::search::SearchDirection::Forward,
        "e".into(),
    );
    app.render_silent();
    let short_common_ms = elapsed_ms(short_common_start.elapsed());
    let short_common_snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot("short_common", &short_common_snapshot);
    }
    assert_search_uses_candidate_index(&short_common_snapshot, "short_common", 512);
    assert_eq!(
        perf_value_max(&short_common_snapshot, "store:transcript:search_fts"),
        0,
        "short_common unexpectedly used FTS"
    );

    smelt_perf::perf::clear();
    let short_absent_start = std::time::Instant::now();
    app.app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        crate::app::search::SearchDirection::Forward,
        "§".into(),
    );
    app.render_silent();
    let short_absent_ms = elapsed_ms(short_absent_start.elapsed());
    let short_absent_snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot("short_absent", &short_absent_snapshot);
    }
    assert_search_uses_candidate_index(&short_absent_snapshot, "short_absent", 0);
    assert_eq!(
        perf_value_max(&short_absent_snapshot, "store:transcript:search_fts"),
        0,
        "short_absent unexpectedly used FTS"
    );

    smelt_perf::perf::clear();
    let common_start = std::time::Instant::now();
    app.app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        crate::app::search::SearchDirection::Forward,
        "common-token".into(),
    );
    app.render_silent();
    let common_submit_ms = elapsed_ms(common_start.elapsed());

    let next_start = std::time::Instant::now();
    for _ in 0..100 {
        app.type_char('n');
        app.render_silent();
    }
    let next100_ms = elapsed_ms(next_start.elapsed());
    let common_snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot("common_hot_next100", &common_snapshot);
    }
    assert_search_refinement_gates(&common_snapshot, "common_hot_next100", 512, 32_000);
    assert_search_uses_candidate_index(&common_snapshot, "common_hot_next100", 512);
    let scanned_entries = perf_value_total(&common_snapshot, "search:transcript:scanned_entries");
    assert!(
        scanned_entries <= 64,
        "common_hot_next100 scanned {scanned_entries} entries; cached next navigation should not rescan every press"
    );

    smelt_perf::perf::clear();
    app.app
        .push_block(smelt_core::transcript_model::Block::Text {
            content: "append after index common-token incremental-index-probe".into(),
        });
    let after_append_start = std::time::Instant::now();
    app.app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        crate::app::search::SearchDirection::Forward,
        "incremental-index-probe".into(),
    );
    app.render_silent();
    let after_append_ms = elapsed_ms(after_append_start.elapsed());
    let after_append_snapshot = smelt_perf::perf::snapshot();
    if report_perf {
        search_perf_snapshot("after_append", &after_append_snapshot);
    }
    assert_search_refinement_gates(&after_append_snapshot, "after_append", 80, 160);
    assert_search_uses_candidate_index(&after_append_snapshot, "after_append", 1);
    let dirty_scanned = perf_value_max(
        &after_append_snapshot,
        "search:transcript:dirty_candidates_scanned",
    );
    assert!(
        dirty_scanned <= 1,
        "after_append scanned {dirty_scanned} dirty blocks, expected only the appended suffix"
    );

    install_sparse_resume_bench_transcript(&mut app);
    let prod_burst_top_ctrl_d80_ms = measure_transcript_burst_operation_without_trace(
        &mut app,
        "prod_burst_top_ctrl_d80",
        report_perf,
        BurstBenchPosition::Top,
        BurstBenchKey::CtrlD,
    );
    let prod_burst_top_down80_ms = measure_transcript_burst_operation_without_trace(
        &mut app,
        "prod_burst_top_down80",
        report_perf,
        BurstBenchPosition::Top,
        BurstBenchKey::Down,
    );
    let prod_burst_bottom_ctrl_u80_ms = measure_transcript_burst_operation_without_trace(
        &mut app,
        "prod_burst_bottom_ctrl_u80",
        report_perf,
        BurstBenchPosition::Bottom,
        BurstBenchKey::CtrlU,
    );
    let prod_burst_bottom_up80_ms = measure_transcript_burst_operation_without_trace(
        &mut app,
        "prod_burst_bottom_up80",
        report_perf,
        BurstBenchPosition::Bottom,
        BurstBenchKey::Up,
    );
    let prod_burst_mid_ctrl_d80_ms = measure_transcript_burst_operation_without_trace(
        &mut app,
        "prod_burst_mid_ctrl_d80",
        report_perf,
        BurstBenchPosition::Middle,
        BurstBenchKey::CtrlD,
    );
    let prod_burst_mid_down80_ms = measure_transcript_burst_operation_without_trace(
        &mut app,
        "prod_burst_mid_down80",
        report_perf,
        BurstBenchPosition::Middle,
        BurstBenchKey::Down,
    );
    let prod_burst_mid_ctrl_u80_ms = measure_transcript_burst_operation_without_trace(
        &mut app,
        "prod_burst_mid_ctrl_u80",
        report_perf,
        BurstBenchPosition::Middle,
        BurstBenchKey::CtrlU,
    );
    let prod_burst_mid_up80_ms = measure_transcript_burst_operation_without_trace(
        &mut app,
        "prod_burst_mid_up80",
        report_perf,
        BurstBenchPosition::Middle,
        BurstBenchKey::Up,
    );

    install_sparse_resume_bench_transcript(&mut app);
    let burst_top_ctrl_d80_ms = measure_transcript_burst_operation(
        &mut app,
        "burst_top_ctrl_d80",
        report_perf,
        BurstBenchPosition::Top,
        BurstBenchKey::CtrlD,
    );
    let burst_top_down80_ms = measure_transcript_burst_operation(
        &mut app,
        "burst_top_down80",
        report_perf,
        BurstBenchPosition::Top,
        BurstBenchKey::Down,
    );
    let burst_bottom_ctrl_u80_ms = measure_transcript_burst_operation(
        &mut app,
        "burst_bottom_ctrl_u80",
        report_perf,
        BurstBenchPosition::Bottom,
        BurstBenchKey::CtrlU,
    );
    let burst_bottom_up80_ms = measure_transcript_burst_operation(
        &mut app,
        "burst_bottom_up80",
        report_perf,
        BurstBenchPosition::Bottom,
        BurstBenchKey::Up,
    );
    let burst_mid_ctrl_d80_ms = measure_transcript_burst_operation(
        &mut app,
        "burst_mid_ctrl_d80",
        report_perf,
        BurstBenchPosition::Middle,
        BurstBenchKey::CtrlD,
    );
    let burst_mid_down80_ms = measure_transcript_burst_operation(
        &mut app,
        "burst_mid_down80",
        report_perf,
        BurstBenchPosition::Middle,
        BurstBenchKey::Down,
    );
    let burst_mid_ctrl_u80_ms = measure_transcript_burst_operation(
        &mut app,
        "burst_mid_ctrl_u80",
        report_perf,
        BurstBenchPosition::Middle,
        BurstBenchKey::CtrlU,
    );
    let burst_mid_up80_ms = measure_transcript_burst_operation(
        &mut app,
        "burst_mid_up80",
        report_perf,
        BurstBenchPosition::Middle,
        BurstBenchKey::Up,
    );

    install_sparse_resume_bench_transcript(&mut app);
    let (sparse_rare_ms, sparse_common_submit_ms, sparse_next100_ms) =
        measure_sparse_search_navigation(&mut app, report_perf);
    smelt_perf::perf::set_enabled(false);

    SearchBenchSample {
        bytes,
        rows,
        width_resize_ms,
        height_resize_ms,
        theme_color_ms,
        copy_mid_ms,
        nav_ctrl_d20_ms,
        nav_ctrl_u20_ms,
        nav_gg_ms,
        nav_g_ms,
        prod_burst_top_ctrl_d80_ms,
        prod_burst_top_down80_ms,
        prod_burst_bottom_ctrl_u80_ms,
        prod_burst_bottom_up80_ms,
        prod_burst_mid_ctrl_d80_ms,
        prod_burst_mid_down80_ms,
        prod_burst_mid_ctrl_u80_ms,
        prod_burst_mid_up80_ms,
        burst_top_ctrl_d80_ms,
        burst_top_down80_ms,
        burst_bottom_ctrl_u80_ms,
        burst_bottom_up80_ms,
        burst_mid_ctrl_d80_ms,
        burst_mid_down80_ms,
        burst_mid_ctrl_u80_ms,
        burst_mid_up80_ms,
        rare_ms,
        short_common_ms,
        short_absent_ms,
        common_submit_ms,
        next100_ms,
        sparse_rare_ms,
        sparse_common_submit_ms,
        sparse_next100_ms,
        after_append_ms,
    }
}

fn resume_bench_session_count() -> usize {
    env_positive_usize("SMELT_RESUME_BENCH_SESSIONS", 1024)
}

fn resume_bench_preview_bytes() -> usize {
    env_positive_usize("SMELT_RESUME_BENCH_PREVIEW_BYTES", 5 * 1024 * 1024)
}

// COMPAT(session-derived-sidecar-exports): wait only for stale/missing export fixtures.
fn wait_for_resume_compatibility_exports(id: &str) -> std::path::PathBuf {
    let session_dir = smelt_core::session::dir_for_id(id);
    let revision = smelt_store::SessionReader::open_existing(&session_dir)
        .unwrap()
        .store_head()
        .unwrap()
        .revision
        .get();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let metadata_revision = std::fs::read(session_dir.join("meta.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| value["source_revision"].as_u64());
        let mut header = String::new();
        let content_revision = std::fs::File::open(session_dir.join("content.txt"))
            .map(std::io::BufReader::new)
            .and_then(|mut reader| {
                std::io::BufRead::read_line(&mut reader, &mut header).map(|_| ())
            })
            .ok()
            .and_then(|()| header.trim_end().strip_prefix("# smelt-revision:"))
            .and_then(|value| value.parse::<u64>().ok());
        if metadata_revision == Some(revision) && content_revision == Some(revision) {
            return session_dir;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "resume benchmark exports did not reach revision {revision}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

// COMPAT(session-derived-sidecar-exports): stale exports must not affect catalog listing.
fn stale_resume_meta(id: &str) {
    let session_dir = wait_for_resume_compatibility_exports(id);
    let meta_path = session_dir.join("meta.json");
    let mut meta_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).expect("read resume bench meta"))
            .expect("parse resume bench meta");
    let object = meta_json.as_object_mut().expect("resume bench meta object");
    object.remove("history_len");
    object.remove("checkpoint");
    object.remove("text_bytes");
    let temporary = session_dir.join(".resume-bench-meta.tmp");
    std::fs::write(
        &temporary,
        serde_json::to_vec(&meta_json).expect("encode stale resume bench meta"),
    )
    .expect("write stale resume bench meta");
    std::fs::rename(temporary, meta_path).expect("replace stale resume bench meta");
}

// COMPAT(session-derived-sidecar-exports): missing exports must not affect catalog listing.
fn remove_resume_meta(id: &str) {
    let session_dir = wait_for_resume_compatibility_exports(id);
    std::fs::remove_file(session_dir.join("meta.json")).expect("remove resume bench meta");
}

fn seed_resume_bench_session(id: String, updated_at_ms: u64, target_text_bytes: usize, cwd: &str) {
    let mut session = smelt_core::session::Session::new(4242, std::path::PathBuf::from(cwd));
    session.id = id.clone();
    session.title = Some(format!("resume bench {id}"));
    session.first_user_message = Some(format!("open resume dialog for {id}"));
    session.created_at_ms = updated_at_ms;
    session.updated_at_ms = updated_at_ms;
    let mut bytes = 0usize;
    let mut turn = 0usize;
    while bytes < target_text_bytes {
        let user = format!("resume bench prompt {turn}: {}", "preview seed ".repeat(8));
        bytes += user.len();
        session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text(user)));

        let assistant = format!(
            "# Resume bench reply {turn}\n\n{}\n\n```text\n{}\n```\n",
            "large preview transcript body ".repeat(80),
            "tail preview row ".repeat(80)
        );
        bytes += assistant.len();
        session.history.push(protocol::HistoryItem::Assistant(
            protocol::AssistantStep::terminal(
                Some(protocol::Content::text(assistant)),
                None,
                Vec::new(),
            ),
        ));
        turn += 1;
    }
    smelt_core::session::save(&session);
    remove_resume_meta(&id);
}

fn seed_resume_bench_sessions(count: usize) {
    let base_time = 1_700_000_000_000u64;
    for i in 0..count {
        let id = format!("{i:064x}");
        seed_resume_bench_session(id, base_time + i as u64, 128, "/resume-bench-other");
    }
}

fn seed_resume_bench_preview_session(
    guard: &std::sync::MutexGuard<'static, ()>,
    preview_bytes: usize,
) -> (String, usize) {
    let mut app = TestApp::builder()
        .with_vim(true)
        .build_with_test_home_guard(guard);
    app.app.handle_resize(120, 32);
    let bytes = push_search_bench_transcript(&mut app, preview_bytes);
    app.render_silent();
    save_bench_fixture(&mut app, "resume preview");
    let id = app.app.core.session.id.clone();
    assert!(
        crate::app::history::load_transcript_tail_from_sqlite_id(&id, 80, 12).is_some(),
        "resume bench preview session should have sparse transcript descriptors"
    );
    stale_resume_meta(&id);
    (id, bytes)
}

fn run_resume_command_to_dialog(app: &mut TestApp) -> f64 {
    let start = std::time::Instant::now();
    assert!(app.run_lua(r#"smelt.cmd.run("resume")"#));
    drive_lua_tasks(app);
    app.render_silent();
    elapsed_ms(start.elapsed())
}

fn run_resume_preview_timer(app: &mut TestApp) -> f64 {
    let start = std::time::Instant::now();
    app.feed_one(SourceEvent::Tick(50));
    app.app.tick_timers();
    drive_lua_tasks(app);
    app.render_silent();
    elapsed_ms(start.elapsed())
}

fn duration_count(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> usize {
    snapshot
        .durations
        .iter()
        .find(|row| row.label == label)
        .map(|row| row.count)
        .unwrap_or(0)
}

fn duration_total_us(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> u64 {
    snapshot
        .durations
        .iter()
        .find(|row| row.label == label)
        .map(|row| row.total_us)
        .unwrap_or(0)
}

fn print_resume_perf(label: &str, snapshot: &smelt_perf::perf::Snapshot) {
    for row in snapshot.durations.iter().filter(|row| {
        row.label.starts_with("session:")
            || row.label.starts_with("store:db:")
            || row.label == "cmd:dispatch"
            || row.label == "lua:cmd"
            || row.label == "lua:timer"
    }) {
        eprintln!(
            "RESUME_DIALOG_PERF_DURATION label={} metric={} count={} total_us={} p95_us={} max_us={}",
            label, row.label, row.count, row.total_us, row.p95_us, row.max_us
        );
    }
}

#[test]
#[ignore = "manual resume dialog benchmark; run with SMELT_RESUME_BENCH=1"]
fn resume_dialog_open_benchmark_suite() {
    if std::env::var("SMELT_RESUME_BENCH").ok().as_deref() != Some("1") {
        eprintln!("RESUME_DIALOG_BENCH_SKIPPED");
        return;
    }

    let guard = test_home_guard();
    let count = resume_bench_session_count();
    let preview_bytes = resume_bench_preview_bytes();
    let (latest_id, seeded_preview_bytes) =
        seed_resume_bench_preview_session(&guard, preview_bytes);
    seed_resume_bench_sessions(count.saturating_sub(1));
    let mut app = TestApp::builder()
        .with_vim(true)
        .build_without_test_home_reset(&guard);
    app.app.handle_resize(120, 32);

    smelt_perf::perf::clear();
    smelt_perf::perf::set_enabled(true);
    let open_ms = run_resume_command_to_dialog(&mut app);
    let open_snapshot = smelt_perf::perf::snapshot();
    print_resume_perf("open", &open_snapshot);
    assert!(
        app.state().active_modal.is_some(),
        "resume command did not open a dialog"
    );
    let open_ro = duration_count(&open_snapshot, "store:db:open_read_only");
    assert!(
        open_ro <= 8,
        "opening /resume opened {open_ro} read-only sqlite databases for {count} sessions; listing must not be proportional to session count"
    );
    let open_rw = duration_count(&open_snapshot, "store:db:open_read_write");
    assert!(
        open_rw <= 8,
        "opening /resume opened {open_rw} read-write sqlite databases for {count} sessions; listing must not backfill sidecars on the foreground path"
    );
    assert_eq!(
        duration_count(&open_snapshot, "session:load_full"),
        0,
        "resume preview must use sparse sqlite transcript records, not full session load"
    );
    assert_eq!(duration_count(&open_snapshot, "session:list_page"), 1);

    smelt_perf::perf::clear();
    let preview_ms = run_resume_preview_timer(&mut app);
    let preview_snapshot = smelt_perf::perf::snapshot();
    print_resume_perf("preview", &preview_snapshot);
    smelt_perf::perf::set_enabled(false);
    assert_eq!(
        duration_count(&preview_snapshot, "session:load_full"),
        0,
        "delayed resume preview refresh must not full-load the selected session"
    );
    let preview_render_count = duration_count(&open_snapshot, "session:render_preview_into")
        + duration_count(&preview_snapshot, "session:render_preview_into");
    assert!(
        preview_render_count > 0,
        "resume dialog did not render the selected session preview"
    );

    eprintln!(
        "RESUME_DIALOG_BENCH_SUMMARY sessions={} latest_id={} preview_bytes={} open_ms={:.3} preview_ms={:.3} open_db_ro={} open_db_rw={} open_session_list_us={} preview_render_us={}",
        count,
        latest_id,
        seeded_preview_bytes,
        open_ms,
        preview_ms,
        duration_count(&open_snapshot, "store:db:open_read_only"),
        duration_count(&open_snapshot, "store:db:open_read_write"),
        duration_total_us(&open_snapshot, "session:list_page"),
        duration_total_us(&open_snapshot, "session:render_preview_into")
            + duration_total_us(&preview_snapshot, "session:render_preview_into"),
    );
}

#[test]
#[ignore = "manual large transcript search benchmark; run via `cargo xtask bench-transcript-layout --search`"]
fn transcript_layout_search_benchmark_suite() {
    if std::env::var("SMELT_TRANSCRIPT_BENCH_SEARCH")
        .ok()
        .as_deref()
        != Some("1")
    {
        eprintln!("TRANSCRIPT_SEARCH_BENCH_SKIPPED");
        return;
    }
    let runs = navigation_bench_runs();
    let target_bytes = search_bench_bytes();
    if transcript_bench_warmup_enabled() {
        let _warmup = run_search_bench_sample(target_bytes, false);
    }
    let mut samples = Vec::with_capacity(runs);
    for run in 0..runs {
        let sample = run_search_bench_sample(target_bytes, true);
        let metric_text = search_bench_metric_values(&sample)
            .into_iter()
            .map(|(label, value)| format!("{label}_ms={value:.3}"))
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!(
            "TRANSCRIPT_SEARCH_BENCH_SAMPLE run={} bytes={} rows={} {}",
            run + 1,
            sample.bytes,
            sample.rows,
            metric_text,
        );
        samples.push(sample);
    }

    let metric_stats = search_bench_metric_values(&samples[0])
        .into_iter()
        .enumerate()
        .map(|(index, (label, _))| {
            let values = samples
                .iter()
                .map(|sample| search_bench_metric_values(sample)[index].1)
                .collect::<Vec<_>>();
            (label, NavStats::from(&values))
        })
        .collect::<Vec<_>>();
    let summary_metrics = metric_stats
        .iter()
        .flat_map(|(label, stats)| {
            [
                format!("{label}_mean_ms={:.3}", stats.mean),
                format!("{label}_stddev_ms={:.3}", stats.stddev),
            ]
        })
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!(
        "TRANSCRIPT_SEARCH_BENCH_SUMMARY runs={} bytes={} rows={} {}",
        samples.len(),
        samples[0].bytes,
        samples[0].rows,
        summary_metrics,
    );
    let json_metrics = metric_stats
        .iter()
        .flat_map(|(label, stats)| {
            [
                format!("\"{label}_mean_ms\":{:.3}", stats.mean),
                format!("\"{label}_stddev_ms\":{:.3}", stats.stddev),
            ]
        })
        .collect::<Vec<_>>()
        .join(",");
    eprintln!(
        "TRANSCRIPT_SEARCH_BENCH_JSON {{\"type\":\"search_summary\",\"runs\":{},\"bytes\":{},\"rows\":{},{}}}",
        samples.len(),
        samples[0].bytes,
        samples[0].rows,
        json_metrics,
    );
}

#[test]
#[ignore = "manual transcript navigation/search benchmark suite; prefer `cargo xtask bench-transcript-layout`"]
fn transcript_layout_navigation_benchmark_suite() {
    if std::env::var("SMELT_TRANSCRIPT_BENCH_SKIP_NAV")
        .ok()
        .as_deref()
        == Some("1")
    {
        eprintln!("TRANSCRIPT_LAYOUT_NAV_SKIPPED");
        return;
    }
    let runs = navigation_bench_runs();
    let _warmup = run_navigation_sample();
    let mut samples = Vec::with_capacity(runs);
    for run in 0..runs {
        let sample = run_navigation_sample();
        eprintln!(
            "TRANSCRIPT_LAYOUT_NAV_SAMPLE run={} rows={} search_ms={:.3} ctrl_d20_ms={:.3} ctrl_u20_ms={:.3} gg_ms={:.3} G_ms={:.3}",
            run + 1,
            sample.rows,
            sample.search_ms,
            sample.ctrl_d20_ms,
            sample.ctrl_u20_ms,
            sample.gg_ms,
            sample.g_ms,
        );
        samples.push(sample);
    }

    let search = NavStats::from(
        &samples
            .iter()
            .map(|sample| sample.search_ms)
            .collect::<Vec<_>>(),
    );
    let ctrl_d = NavStats::from(
        &samples
            .iter()
            .map(|sample| sample.ctrl_d20_ms)
            .collect::<Vec<_>>(),
    );
    let ctrl_u = NavStats::from(
        &samples
            .iter()
            .map(|sample| sample.ctrl_u20_ms)
            .collect::<Vec<_>>(),
    );
    let gg = NavStats::from(
        &samples
            .iter()
            .map(|sample| sample.gg_ms)
            .collect::<Vec<_>>(),
    );
    let g = NavStats::from(&samples.iter().map(|sample| sample.g_ms).collect::<Vec<_>>());
    eprintln!(
        "TRANSCRIPT_LAYOUT_NAV_SUMMARY runs={} rows={} search_mean_ms={:.3} search_stddev_ms={:.3} ctrl_d20_mean_ms={:.3} ctrl_d20_stddev_ms={:.3} ctrl_u20_mean_ms={:.3} ctrl_u20_stddev_ms={:.3} gg_mean_ms={:.3} gg_stddev_ms={:.3} G_mean_ms={:.3} G_stddev_ms={:.3}",
        samples.len(),
        samples[0].rows,
        search.mean,
        search.stddev,
        ctrl_d.mean,
        ctrl_d.stddev,
        ctrl_u.mean,
        ctrl_u.stddev,
        gg.mean,
        gg.stddev,
        g.mean,
        g.stddev,
    );
    eprintln!(
        "TRANSCRIPT_LAYOUT_NAV_JSON {{\"type\":\"navigation_summary\",\"runs\":{},\"rows\":{},\"search_mean_ms\":{:.3},\"search_stddev_ms\":{:.3},\"ctrl_d20_mean_ms\":{:.3},\"ctrl_d20_stddev_ms\":{:.3},\"ctrl_u20_mean_ms\":{:.3},\"ctrl_u20_stddev_ms\":{:.3},\"gg_mean_ms\":{:.3},\"gg_stddev_ms\":{:.3},\"G_mean_ms\":{:.3},\"G_stddev_ms\":{:.3}}}",
        samples.len(),
        samples[0].rows,
        search.mean,
        search.stddev,
        ctrl_d.mean,
        ctrl_d.stddev,
        ctrl_u.mean,
        ctrl_u.stddev,
        gg.mean,
        gg.stddev,
        g.mean,
        g.stddev,
    );
    eprintln!(
        "| navigation/search | rows={} | search={}ms | ctrl-d×20={}ms | ctrl-u×20={}ms | gg={}ms | G={}ms |",
        samples[0].rows,
        search.display(),
        ctrl_d.display(),
        ctrl_u.display(),
        gg.display(),
        g.display(),
    );
}

#[derive(Clone, Copy, Debug)]
struct HotPathCounters {
    history_suffix_rows: u64,
    history_inserted: u64,
    history_deleted: u64,
    descriptor_suffix_rows: u64,
    descriptor_inserted: u64,
    descriptor_deleted: u64,
    read_range_rows: u64,
    cached_read_write_db: u64,
    invariant_history_rows: u64,
    search_blob_rows: u64,
    search_blob_bytes: u64,
    user_turn_blocks_scanned: u64,
    user_turns_cloned: u64,
    user_turn_text_bytes_cloned: u64,
}

impl HotPathCounters {
    fn from(snapshot: &smelt_perf::perf::Snapshot) -> Self {
        Self {
            history_suffix_rows: perf_value_max(snapshot, "store:history:dirty_suffix_rows"),
            history_inserted: perf_value_max(snapshot, "store:session:history_rows_inserted"),
            history_deleted: perf_value_max(snapshot, "store:session:history_rows_deleted"),
            descriptor_suffix_rows: perf_value_max(
                snapshot,
                "store:transcript:dirty_descriptor_suffix_rows",
            ),
            descriptor_inserted: perf_value_max(
                snapshot,
                "store:transcript:descriptor_db_rows_inserted",
            ),
            descriptor_deleted: perf_value_max(
                snapshot,
                "store:transcript:descriptor_db_rows_deleted",
            ),
            read_range_rows: perf_value_max(snapshot, "store:history:read_range_rows"),
            cached_read_write_db: perf_value_max(snapshot, "store:db:cached_read_write"),
            invariant_history_rows: perf_value_max(
                snapshot,
                "store:session:invariant_history_rows",
            ),
            search_blob_rows: perf_value_max(snapshot, "store:transcript:search_blob_rows_read"),
            search_blob_bytes: perf_value_max(snapshot, "store:transcript:search_blob_bytes_read"),
            user_turn_blocks_scanned: perf_value_max(
                snapshot,
                "transcript:last_user_block_index:blocks_scanned",
            )
            .max(perf_value_max(
                snapshot,
                "transcript:user_turns:blocks_scanned",
            )),
            user_turns_cloned: perf_value_max(snapshot, "transcript:user_turns:users_cloned"),
            user_turn_text_bytes_cloned: perf_value_max(
                snapshot,
                "transcript:user_turns:text_bytes_cloned",
            ),
        }
    }
}

#[derive(Clone, Debug)]
struct HotPathSample {
    operation: &'static str,
    history_len: usize,
    history_item_bytes: usize,
    ms: f64,
    thread_allocs: u64,
    thread_bytes_allocated: u64,
    process_bytes_allocated: u64,
    process_bytes_deallocated: u64,
    process_current_bytes_before: usize,
    process_current_bytes_after: usize,
    process_retained_bytes: i64,
    counters: HotPathCounters,
}

fn hot_path_enabled() -> bool {
    matches!(
        std::env::var("SMELT_TRANSCRIPT_HOT_PATH").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn hot_path_history_len() -> usize {
    std::env::var("SMELT_TRANSCRIPT_HOT_PATH_HISTORY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|rows| *rows >= 4)
        .unwrap_or(1024)
}

fn hot_path_history_item_bytes() -> usize {
    std::env::var("SMELT_TRANSCRIPT_HOT_PATH_ITEM_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0)
}

fn hot_path_session_id(_label: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:032x}{:016x}{counter:016x}", std::process::id())
}

fn hot_path_user(text: &str) -> protocol::HistoryItem {
    protocol::HistoryItem::user(protocol::Content::text(text))
}

fn hot_path_assistant(text: &str) -> protocol::HistoryItem {
    protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
        Some(protocol::Content::text(text)),
        None,
        Vec::new(),
    ))
}

fn hot_path_history_item(idx: usize, target_bytes: usize) -> protocol::HistoryItem {
    let prefix = if idx.is_multiple_of(2) {
        format!("hot path old user {idx}")
    } else {
        format!("hot path old assistant {idx}")
    };
    let mut text = String::with_capacity(prefix.len().max(target_bytes));
    text.push_str(&prefix);
    text.extend(std::iter::repeat_n(
        'x',
        target_bytes.saturating_sub(prefix.len()),
    ));
    if idx.is_multiple_of(2) {
        hot_path_user(&text)
    } else {
        hot_path_assistant(&text)
    }
}

fn saved_hot_path_app(
    label: &str,
    history_len: usize,
    checkpoint_first_live: Option<usize>,
) -> TestApp {
    let mut app = TestApp::builder().build();
    let mut session =
        smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
    session.id = hot_path_session_id(label);
    session.first_user_message = Some("hot path old user 0".into());
    let item_bytes = hot_path_history_item_bytes();
    session.history = (0..history_len)
        .map(|idx| hot_path_history_item(idx, item_bytes))
        .collect();
    if let Some(first_live_index) = checkpoint_first_live {
        session.checkpoint = Some(smelt_core::ContextCheckpoint {
            kind: "benchmark".into(),
            summary: "checkpointed benchmark prefix".into(),
            first_live_index: first_live_index.min(history_len),
            created_at_ms: smelt_core::session::now_ms(),
            tokens_before: Some(10_000),
            tokens_after_estimate: Some(1_000),
            tokens_after_estimate_history_len: Some(history_len),
            pre_checkpoint_context_tokens: None,
            pre_checkpoint_context_history_len: None,
        });
    }

    app.app.load_session(session);
    app.app.restore_screen();
    save_bench_fixture(&mut app, "hot path");
    app
}

fn assert_no_full_store_hot_path_reads(snapshot: &smelt_perf::perf::Snapshot, operation: &str) {
    for metric in [
        "store:history:read_all",
        "store:session:load_full_snapshot",
        "store:transcript:read_descriptors_full",
    ] {
        let count = perf_duration_count(snapshot, metric);
        assert_eq!(
            count, 0,
            "{operation} recorded {metric} {count} times, expected no full-store hot-path work"
        );
    }
    for metric in [
        "store:history:read_all_rows",
        "store:session:full_snapshot_rows_read",
        "store:transcript:descriptors_full_loaded",
        "transcript:build_from_session:history_items",
    ] {
        let value = perf_value_max(snapshot, metric);
        assert_eq!(
            value, 0,
            "{operation} recorded {metric}={value}, expected no full-store hot-path work"
        );
    }
}

fn assert_no_full_hot_path_reads(snapshot: &smelt_perf::perf::Snapshot, operation: &str) {
    assert_no_full_store_hot_path_reads(snapshot, operation);
    let fingerprint_count = perf_duration_count(snapshot, "transcript:render_plan:fingerprint");
    assert_eq!(
        fingerprint_count, 0,
        "{operation} rebuilt {fingerprint_count} full transcript render-plan fingerprints"
    );
}

fn assert_hot_path_at_most(
    snapshot: &smelt_perf::perf::Snapshot,
    operation: &str,
    metric: &str,
    max: u64,
) {
    let value = perf_value_max(snapshot, metric);
    assert!(
        value <= max,
        "{operation} recorded {metric}={value}, expected <= {max}"
    );
}

fn assert_cached_persist_db(snapshot: &smelt_perf::perf::Snapshot, operation: &str) {
    let open_us = perf_duration_max(snapshot, "store:db:open_read_write");
    assert_eq!(
        open_us, 0,
        "{operation} reopened the session database in {open_us}us instead of reusing the persist worker connection"
    );
    assert_eq!(
        perf_value_max(snapshot, "store:db:cached_read_write"),
        1,
        "{operation} did not reuse the persist worker database connection"
    );
}

fn capture_hot_path_sample(
    operation: &'static str,
    history_len: usize,
    body: impl FnOnce(),
) -> (HotPathSample, smelt_perf::perf::Snapshot) {
    smelt_perf::perf::clear();
    smelt_perf::perf::set_enabled(true);
    let process_before = smelt_perf::alloc::snapshot();
    let (allocs_before, bytes_before) = smelt_perf::alloc::thread_snapshot();
    let start = std::time::Instant::now();
    body();
    let ms = elapsed_ms(start.elapsed());
    let (allocs_after, bytes_after) = smelt_perf::alloc::thread_snapshot();
    let process_after = smelt_perf::alloc::snapshot();
    let process_delta = smelt_perf::alloc::delta(process_before, process_after);
    let snapshot = smelt_perf::perf::snapshot();
    smelt_perf::perf::set_enabled(false);
    let sample = HotPathSample {
        operation,
        history_len,
        history_item_bytes: hot_path_history_item_bytes(),
        ms,
        thread_allocs: allocs_after.saturating_sub(allocs_before),
        thread_bytes_allocated: bytes_after.saturating_sub(bytes_before),
        process_bytes_allocated: process_delta.bytes_allocated,
        process_bytes_deallocated: process_delta.bytes_deallocated,
        process_current_bytes_before: process_before.current_bytes,
        process_current_bytes_after: process_after.current_bytes,
        process_retained_bytes: process_after.current_bytes as i64
            - process_before.current_bytes as i64,
        counters: HotPathCounters::from(&snapshot),
    };
    (sample, snapshot)
}

fn read_provider_history_source(
    source: protocol::ModelHistorySource,
    session_dir: &std::path::Path,
) -> Vec<protocol::HistoryItem> {
    match source {
        protocol::ModelHistorySource::Items { items, .. } => items,
        protocol::ModelHistorySource::Store {
            prefix,
            first_live_index,
            end_index,
            suffix,
            ..
        } => {
            smelt_perf::perf::record_value("engine:model_history:source_store", 1);
            smelt_perf::perf::record_value(
                "engine:model_history:first_live_index",
                first_live_index as u64,
            );
            smelt_perf::perf::record_value("engine:model_history:end_index", end_index as u64);
            smelt_perf::perf::record_value(
                "engine:model_history:suffix_items",
                suffix.len() as u64,
            );
            let mut history = prefix;
            if end_index > first_live_index {
                let db = smelt_store::SessionDb::open_read_only(session_dir.join("session.db"))
                    .expect("open provider history database");
                let mut rows = db
                    .read_history_items_range(first_live_index..end_index)
                    .expect("read provider history rows");
                smelt_perf::perf::record_value("engine:model_history:rows_read", rows.len() as u64);
                history.append(&mut rows);
            }
            history.extend(suffix);
            smelt_perf::perf::record_value("engine:model_history:items", history.len() as u64);
            history
        }
    }
}

fn run_noop_save_hot_path(history_len: usize) -> (HotPathSample, smelt_perf::perf::Snapshot) {
    let mut app = saved_hot_path_app("noop-save", history_len, None);
    let (sample, snapshot) = capture_hot_path_sample("noop_save", history_len, || {
        app.app.save_session();
        app.app.flush_persist();
    });
    assert_eq!(
        perf_value_max(&snapshot, "session:save:skipped_unchanged"),
        1,
        "no-op save did not take the unchanged fast path"
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:history:dirty_suffix_rows",
        0,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:transcript:dirty_descriptor_suffix_rows",
        0,
    );
    assert_no_full_hot_path_reads(&snapshot, sample.operation);
    (sample, snapshot)
}

fn run_request_append_hot_path(history_len: usize) -> (HotPathSample, smelt_perf::perf::Snapshot) {
    let mut app = saved_hot_path_app("request-append", history_len, None);
    let (sample, snapshot) = capture_hot_path_sample("request_append", history_len, || {
        let source = app.app.commit_request_history_item(
            hot_path_user("hot path new user"),
            Some(smelt_core::Block::User {
                text: "hot path new user".into(),
                image_labels: vec![],
                command: false,
            }),
        );
        assert!(matches!(
            source,
            protocol::ModelHistorySource::Store { end_index, .. } if end_index == history_len
        ));
    });
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:history:dirty_suffix_rows",
        1,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:session:history_rows_inserted",
        1,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:transcript:dirty_descriptor_suffix_rows",
        1,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:transcript:descriptor_db_rows_inserted",
        1,
    );
    assert_no_full_hot_path_reads(&snapshot, sample.operation);
    assert_cached_persist_db(&snapshot, sample.operation);
    (sample, snapshot)
}

fn run_history_appended_hot_path(
    history_len: usize,
) -> (HotPathSample, smelt_perf::perf::Snapshot) {
    let mut app = saved_hot_path_app("history-appended", history_len, None);
    app.start_turn(1);
    let item = hot_path_assistant("hot path assistant update");
    let (sample, snapshot) = capture_hot_path_sample("history_appended", history_len, || {
        app.app
            .dispatch_engine_event(protocol::EngineEvent::HistoryAppended {
                turn_id: 1,
                delta: protocol::CanonicalHistoryDelta::new(history_len, vec![item]),
            });
        app.app.flush_persist();
    });
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:history:dirty_suffix_rows",
        1,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:session:history_rows_inserted",
        1,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "tui:set_history:dirty_prefix_compared",
        0,
    );
    assert_no_full_hot_path_reads(&snapshot, sample.operation);
    assert_cached_persist_db(&snapshot, sample.operation);
    (sample, snapshot)
}

fn run_turn_complete_hot_path(history_len: usize) -> (HotPathSample, smelt_perf::perf::Snapshot) {
    let mut app = saved_hot_path_app("turn-complete", history_len, None);
    app.start_turn(1);
    let meta = protocol::TurnMeta {
        elapsed_ms: 1,
        avg_tps: None,
        display_tps: None,
        interrupted: false,
        tool_elapsed: std::collections::HashMap::new(),
    };
    let (sample, snapshot) = capture_hot_path_sample("turn_complete", history_len, || {
        app.app
            .dispatch_engine_event(protocol::EngineEvent::TurnComplete {
                turn_id: 1,
                history: None,
                meta: Some(meta),
            });
        app.app.flush_persist();
    });
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:history:dirty_suffix_rows",
        0,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "tui:set_history:dirty_prefix_compared",
        0,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "tui:set_history:dirty_from_hint",
        0,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "lua:session:conversation_rows_scanned",
        16,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "lua:session:conversation_rows_returned",
        16,
    );
    assert!(
        app.app.session_document.has_session_work(),
        "{} did not defer completion metadata for the next save point",
        sample.operation
    );
    assert_eq!(
        perf_duration_max(&snapshot, "session:save"),
        0,
        "{} saved low-priority completion metadata in the engine-event hot path",
        sample.operation
    );
    assert_eq!(
        perf_duration_max(&snapshot, "persist:write_metadata"),
        0,
        "{} wrote low-priority completion metadata in the engine-event hot path",
        sample.operation
    );
    assert_eq!(
        perf_duration_max(&snapshot, "store:db:transaction_commit"),
        0,
        "{} committed low-priority completion metadata in the engine-event hot path",
        sample.operation
    );
    assert_no_full_hot_path_reads(&snapshot, sample.operation);
    (sample, snapshot)
}

fn run_rewind_delete_hot_path(history_len: usize) -> (HotPathSample, smelt_perf::perf::Snapshot) {
    let mut app = saved_hot_path_app("rewind-delete", history_len, None);
    let rewind_block = history_len.saturating_sub(2);
    let expected_deleted = history_len.saturating_sub(rewind_block) as u64;
    let (sample, snapshot) = capture_hot_path_sample("rewind_delete_suffix", history_len, || {
        let _ = app.app.rewind_to(rewind_block);
        app.app.save_session();
        app.app.flush_persist();
    });
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:history:dirty_suffix_rows",
        0,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:session:history_rows_deleted",
        expected_deleted,
    );
    assert_no_full_hot_path_reads(&snapshot, sample.operation);
    assert_cached_persist_db(&snapshot, sample.operation);
    (sample, snapshot)
}

fn run_provider_history_hot_path(
    history_len: usize,
) -> (HotPathSample, smelt_perf::perf::Snapshot) {
    let first_live = history_len.saturating_sub(32);
    let app = saved_hot_path_app("provider-history", history_len, Some(first_live));
    let source = app.app.model_history_source();
    let session_dir = smelt_core::session::dir_for(&app.app.core.session);
    let (sample, snapshot) = capture_hot_path_sample("provider_history_read", history_len, || {
        let history = read_provider_history_source(source, &session_dir);
        assert_eq!(history.len(), history_len - first_live + 1);
    });
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:history:read_range_rows",
        (history_len - first_live) as u64,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "engine:model_history:rows_read",
        (history_len - first_live) as u64,
    );
    assert_no_full_hot_path_reads(&snapshot, sample.operation);
    (sample, snapshot)
}

fn run_uncheckpointed_provider_history_hot_path(
    history_len: usize,
) -> (HotPathSample, smelt_perf::perf::Snapshot) {
    let app = saved_hot_path_app("provider-history-uncheckpointed", history_len, None);
    let source = app.app.model_history_source();
    let session_dir = smelt_core::session::dir_for(&app.app.core.session);
    let (sample, snapshot) =
        capture_hot_path_sample("provider_history_uncheckpointed_read", history_len, || {
            let history = read_provider_history_source(source, &session_dir);
            assert_eq!(history.len(), history_len);
        });
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:history:read_range_rows",
        history_len as u64,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "engine:model_history:rows_read",
        history_len as u64,
    );
    assert_no_full_hot_path_reads(&snapshot, sample.operation);
    (sample, snapshot)
}

fn run_engine_request_materialization_hot_path(
    history_len: usize,
) -> (HotPathSample, smelt_perf::perf::Snapshot) {
    let app = saved_hot_path_app("engine-request-materialization", history_len, None);
    let source = app.app.model_history_source();
    let session_dir = smelt_core::session::dir_for(&app.app.core.session);
    let (sample, snapshot) =
        capture_hot_path_sample("engine_request_materialization", history_len, || {
            let history = read_provider_history_source(source, &session_dir);
            let mut engine_history = {
                let _perf = smelt_perf::perf::begin("bench:engine_request:install_history");
                let mut installed = Vec::with_capacity(history.len() + 1);
                installed.push(protocol::HistoryItem::system("benchmark system prompt"));
                installed.extend(history);
                installed
            };
            assert_eq!(engine_history.len(), history_len + 1);

            let prepared = {
                let _perf = smelt_perf::perf::begin("bench:engine_request:prepare_messages");
                engine::PreparedRequestMessages::new(
                    protocol::history_to_messages(&engine_history),
                    1,
                )
            };
            {
                let _perf =
                    smelt_perf::perf::begin("bench:engine_request:estimate_tokens_serialize");
                serde_json::to_writer(std::io::sink(), prepared.model())
                    .expect("measure serialized request view");
            }
            smelt_perf::perf::record_value(
                "bench:engine_request:prepare_message_count",
                prepared.model().len() as u64,
            );
            smelt_perf::perf::record_value(
                "bench:engine_request:provider_message_count",
                prepared.wire().len() as u64,
            );
            std::hint::black_box((&mut engine_history, prepared));
        });
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:history:read_range_rows",
        history_len as u64,
    );
    assert_no_full_hot_path_reads(&snapshot, sample.operation);
    (sample, snapshot)
}

fn run_submit_hot_paths(history_len: usize) -> Vec<(HotPathSample, smelt_perf::perf::Snapshot)> {
    let mut app = saved_hot_path_app("submit", history_len, None);
    app.app.handle_resize(100, 32);
    app.render_silent();
    app.type_text("hot path submitted user message");
    app.clear_actions();

    let (submit, submit_snapshot) = capture_hot_path_sample("submit_enter", history_len, || {
        app.press(KeyCode::Enter);
    });
    let started = app.actions().iter().find_map(|action| match action {
        Action::EngineSend(command) => match command.as_ref() {
            protocol::UiCommand::StartTurn(payload) => Some(payload),
            _ => None,
        },
        _ => None,
    });
    let Some(started) = started else {
        panic!("submit_enter did not dispatch StartTurn");
    };
    assert!(matches!(
        &started.history,
        protocol::ModelHistorySource::Store { .. }
    ));
    assert_hot_path_at_most(
        &submit_snapshot,
        submit.operation,
        "store:history:dirty_suffix_rows",
        2,
    );
    assert_hot_path_at_most(
        &submit_snapshot,
        submit.operation,
        "store:transcript:dirty_descriptor_suffix_rows",
        1,
    );
    assert_cached_persist_db(&submit_snapshot, submit.operation);

    let (render, render_snapshot) =
        capture_hot_path_sample("submit_first_render", history_len, || {
            app.render_silent();
        });
    assert_no_full_store_hot_path_reads(&render_snapshot, render.operation);

    vec![(submit, submit_snapshot), (render, render_snapshot)]
}

fn print_hot_path_perf(operation: &str, snapshot: &smelt_perf::perf::Snapshot) {
    let included = |label: &str| {
        [
            "agent:",
            "app:",
            "session:",
            "persist:",
            "store:",
            "engine:",
            "provider:",
            "transcript:",
            "tui:",
            "lua:",
            "bench:",
        ]
        .iter()
        .any(|prefix| label.starts_with(prefix))
    };
    for row in snapshot.durations.iter().filter(|row| included(row.label)) {
        eprintln!(
            "TRANSCRIPT_HOT_PATH_PERF_DURATION operation={} metric={} count={} last_us={} total_us={} p95_us={} max_us={}",
            operation, row.label, row.count, row.last_us, row.total_us, row.p95_us, row.max_us
        );
    }
    for row in snapshot.values.iter().filter(|row| included(row.label)) {
        eprintln!(
            "TRANSCRIPT_HOT_PATH_PERF_VALUE operation={} metric={} count={} last={} total={} p95={} max={}",
            operation, row.label, row.count, row.last, row.total, row.p95, row.max
        );
    }
    for row in snapshot.allocs.iter().filter(|row| included(row.label)) {
        eprintln!(
            "TRANSCRIPT_HOT_PATH_PERF_ALLOC operation={} metric={} count={} allocs_last={} allocs_total={} bytes_last={} bytes_total={} bytes_p95={} bytes_max={}",
            operation,
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

fn print_hot_path_sample(run: usize, sample: &HotPathSample) {
    let c = sample.counters;
    eprintln!(
        "TRANSCRIPT_HOT_PATH_BENCH_SAMPLE run={} operation={} history_len={} history_item_bytes={} ms={:.3} thread_allocs={} thread_bytes_allocated={} process_bytes_allocated={} process_bytes_deallocated={} process_current_bytes_before={} process_current_bytes_after={} process_retained_bytes={} history_suffix_rows={} history_inserted={} history_deleted={} descriptor_suffix_rows={} descriptor_inserted={} descriptor_deleted={} read_range_rows={} cached_read_write_db={} invariant_history_rows={} search_blob_rows={} search_blob_bytes={} user_turn_blocks_scanned={} user_turns_cloned={} user_turn_text_bytes_cloned={}",
        run,
        sample.operation,
        sample.history_len,
        sample.history_item_bytes,
        sample.ms,
        sample.thread_allocs,
        sample.thread_bytes_allocated,
        sample.process_bytes_allocated,
        sample.process_bytes_deallocated,
        sample.process_current_bytes_before,
        sample.process_current_bytes_after,
        sample.process_retained_bytes,
        c.history_suffix_rows,
        c.history_inserted,
        c.history_deleted,
        c.descriptor_suffix_rows,
        c.descriptor_inserted,
        c.descriptor_deleted,
        c.read_range_rows,
        c.cached_read_write_db,
        c.invariant_history_rows,
        c.search_blob_rows,
        c.search_blob_bytes,
        c.user_turn_blocks_scanned,
        c.user_turns_cloned,
        c.user_turn_text_bytes_cloned,
    );
    eprintln!(
        "TRANSCRIPT_HOT_PATH_BENCH_JSON {{\"type\":\"hot_path_sample\",\"run\":{},\"operation\":\"{}\",\"history_len\":{},\"history_item_bytes\":{},\"ms\":{:.3},\"thread_allocs\":{},\"thread_bytes_allocated\":{},\"process_bytes_allocated\":{},\"process_bytes_deallocated\":{},\"process_current_bytes_before\":{},\"process_current_bytes_after\":{},\"process_retained_bytes\":{},\"history_suffix_rows\":{},\"history_inserted\":{},\"history_deleted\":{},\"descriptor_suffix_rows\":{},\"descriptor_inserted\":{},\"descriptor_deleted\":{},\"read_range_rows\":{},\"cached_read_write_db\":{},\"invariant_history_rows\":{},\"search_blob_rows\":{},\"search_blob_bytes\":{},\"user_turn_blocks_scanned\":{},\"user_turns_cloned\":{},\"user_turn_text_bytes_cloned\":{}}}",
        run,
        sample.operation,
        sample.history_len,
        sample.history_item_bytes,
        sample.ms,
        sample.thread_allocs,
        sample.thread_bytes_allocated,
        sample.process_bytes_allocated,
        sample.process_bytes_deallocated,
        sample.process_current_bytes_before,
        sample.process_current_bytes_after,
        sample.process_retained_bytes,
        c.history_suffix_rows,
        c.history_inserted,
        c.history_deleted,
        c.descriptor_suffix_rows,
        c.descriptor_inserted,
        c.descriptor_deleted,
        c.read_range_rows,
        c.cached_read_write_db,
        c.invariant_history_rows,
        c.search_blob_rows,
        c.search_blob_bytes,
        c.user_turn_blocks_scanned,
        c.user_turns_cloned,
        c.user_turn_text_bytes_cloned,
    );
}

#[test]
#[ignore = "manual transcript submit/save/request hot-path benchmark suite; prefer `cargo xtask bench-transcript-layout`"]
fn transcript_layout_hot_path_benchmark_suite() {
    if !hot_path_enabled() {
        eprintln!("TRANSCRIPT_HOT_PATH_BENCH_SKIPPED set SMELT_TRANSCRIPT_HOT_PATH=1 to run");
        return;
    }

    let runs = navigation_bench_runs();
    let history_len = hot_path_history_len();
    let mut samples = Vec::new();
    for run in 1..=runs {
        let mut run_samples = vec![
            run_noop_save_hot_path(history_len),
            run_request_append_hot_path(history_len),
            run_history_appended_hot_path(history_len),
            run_turn_complete_hot_path(history_len),
            run_rewind_delete_hot_path(history_len),
            run_provider_history_hot_path(history_len),
            run_uncheckpointed_provider_history_hot_path(history_len),
            run_engine_request_materialization_hot_path(history_len),
        ];
        run_samples.extend(run_submit_hot_paths(history_len));
        for (sample, snapshot) in run_samples {
            print_hot_path_perf(sample.operation, &snapshot);
            print_hot_path_sample(run, &sample);
            samples.push(sample);
        }
    }

    for operation in [
        "noop_save",
        "request_append",
        "history_appended",
        "turn_complete",
        "rewind_delete_suffix",
        "provider_history_read",
        "provider_history_uncheckpointed_read",
        "engine_request_materialization",
        "submit_enter",
        "submit_first_render",
    ] {
        let operation_samples = samples
            .iter()
            .filter(|sample| sample.operation == operation)
            .map(|sample| sample.ms)
            .collect::<Vec<_>>();
        let stats = NavStats::from(&operation_samples);
        eprintln!(
            "TRANSCRIPT_HOT_PATH_BENCH_SUMMARY operation={} runs={} history_len={} history_item_bytes={} mean_ms={:.3} stddev_ms={:.3}",
            operation,
            operation_samples.len(),
            history_len,
            hot_path_history_item_bytes(),
            stats.mean,
            stats.stddev,
        );
        eprintln!(
            "TRANSCRIPT_HOT_PATH_BENCH_SUMMARY_JSON {{\"type\":\"hot_path_summary\",\"operation\":\"{}\",\"runs\":{},\"history_len\":{},\"history_item_bytes\":{},\"mean_ms\":{:.3},\"stddev_ms\":{:.3}}}",
            operation,
            operation_samples.len(),
            history_len,
            hot_path_history_item_bytes(),
            stats.mean,
            stats.stddev,
        );
    }
}

fn active_memory_bench_bytes() -> usize {
    env_positive_usize("SMELT_TRANSCRIPT_ACTIVE_MEMORY_BYTES", 50 * 1024 * 1024)
}

fn linux_memory_bytes(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let value_kib = status
        .lines()
        .find_map(|line| line.strip_prefix(field))?
        .split_ascii_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    Some(value_kib.saturating_mul(1024))
}

#[test]
#[ignore = "manual active transcript retained-memory benchmark; run via `cargo xtask bench-transcript-layout --active-memory`"]
fn transcript_active_memory_benchmark_suite() {
    const BLOCK_BYTES: usize = 32 * 1024;
    const SAVE_BATCH_BLOCKS: usize = 256;
    const SEARCH_TARGET: &str = "active-memory-unique-search-target";

    let target_bytes = active_memory_bench_bytes();
    smelt_perf::perf::clear();
    smelt_perf::perf::set_enabled(true);
    let process_start = smelt_perf::alloc::snapshot();
    let started_at = std::time::Instant::now();
    let body = "active transcript canonical payload with markdown wrapping and exact hydration. "
        .repeat(BLOCK_BYTES / 72);
    let mut app = TestApp::builder().with_vim(true).build();
    app.app.handle_resize(100, 32);
    let mut generated_bytes = 0usize;
    let mut block_count = 0usize;
    let mut batch_count = 0usize;
    let mut marker_written = false;

    while generated_bytes < target_bytes {
        let batch_end = block_count.saturating_add(SAVE_BATCH_BLOCKS);
        while generated_bytes < target_bytes && block_count < batch_end {
            let marker = if !marker_written && generated_bytes >= target_bytes / 2 {
                marker_written = true;
                SEARCH_TARGET
            } else {
                "ordinary-active-memory-block"
            };
            let content = format!("# Active block {block_count}\n\n{marker}\n\n{body}");
            generated_bytes = generated_bytes.saturating_add(content.len());
            app.app
                .push_block(smelt_core::transcript_model::Block::Text { content });
            block_count += 1;
        }
        save_bench_fixture(&mut app, "active memory");
        while app.app.session_document.transcript.drain_compaction_slice() {}
        batch_count += 1;
    }
    assert!(marker_written);
    let seeded_ms = elapsed_ms(started_at.elapsed());
    let after_compaction = app.app.session_document.transcript.memory_snapshot();
    assert_eq!(after_compaction.live_blocks, 0);
    assert_eq!(after_compaction.stored_blocks, block_count);
    assert_eq!(after_compaction.hydrated_blocks, 0);
    assert_eq!(
        after_compaction.dematerialized_entries as usize,
        block_count
    );

    app.app.app_focus = AppFocus::Content;
    app.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
    app.app.transcript_win_mut().set_vim_enabled(true);
    app.app.transcript_win_mut().set_vim_mode(VimMode::Normal);
    let render_started_at = std::time::Instant::now();
    app.render_silent();
    let first_render_ms = elapsed_ms(render_started_at.elapsed());

    app.type_char('g');
    app.type_char('g');
    app.render_silent();
    let scroll_started_at = std::time::Instant::now();
    for _ in 0..20 {
        app.press_mod(KeyCode::Char('d'), KeyModifiers::CONTROL);
        app.render_silent();
    }
    let scroll_20_ms = elapsed_ms(scroll_started_at.elapsed());

    let search_started_at = std::time::Instant::now();
    app.app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        crate::app::search::SearchDirection::Forward,
        SEARCH_TARGET.to_string(),
    );
    app.render_silent();
    let search_ms = elapsed_ms(search_started_at.elapsed());
    let next_started_at = std::time::Instant::now();
    app.type_char('n');
    app.render_silent();
    let next_ms = elapsed_ms(next_started_at.elapsed());

    let churn_ids = app
        .app
        .session_document
        .transcript
        .history()
        .order
        .iter()
        .copied()
        .take(1_200)
        .collect::<Vec<_>>();
    let churn_started_at = std::time::Instant::now();
    for id in &churn_ids {
        assert!(app
            .app
            .session_document
            .transcript
            .ensure_hydrated_ids(&[*id]));
    }
    let hydration_churn_ms = elapsed_ms(churn_started_at.elapsed());
    let reads_before_reuse = app
        .app
        .session_document
        .transcript
        .memory_snapshot()
        .hydration_reads;
    if let Some(id) = churn_ids.last() {
        assert!(app
            .app
            .session_document
            .transcript
            .ensure_hydrated_ids(&[*id]));
    }
    let working_set_rereads = app
        .app
        .session_document
        .transcript
        .memory_snapshot()
        .hydration_reads
        .saturating_sub(reads_before_reuse);
    assert_eq!(working_set_rereads, 0);

    let memory = app.app.session_document.transcript.memory_snapshot();
    let process_end = smelt_perf::alloc::snapshot();
    let process_delta = smelt_perf::alloc::delta(process_start, process_end);
    let hydrated_bytes = memory
        .hydrated_block_bytes
        .saturating_add(memory.hydrated_tool_state_bytes);
    let rendered_bytes = memory
        .layout_bytes
        .saturating_add(memory.source_view_bytes)
        .saturating_add(memory.height_index_bytes)
        .saturating_add(memory.height_index_cache_bytes)
        .saturating_add(memory.visible_rows_bytes)
        .saturating_add(memory.full_rows_bytes);
    assert_eq!(memory.live_block_bytes, 0);
    assert_eq!(memory.live_tool_state_bytes, 0);
    assert!(
        hydrated_bytes
            <= memory
                .hydrated_budget_bytes
                .saturating_add(memory.pinned_hydrated_bytes)
                .saturating_add(memory.hydrated_oversize_debt_bytes),
        "hydrated cache exceeded budget plus pins and oversize debt: {memory:?}"
    );
    assert!(
        memory.descriptor_window_bytes
            <= memory
                .descriptor_budget_bytes
                .saturating_add(memory.descriptor_oversize_debt_bytes),
        "descriptor cache exceeded its measured bound: {memory:?}"
    );
    assert!(
        rendered_bytes
            <= memory
                .rendered_budget_bytes
                .saturating_add(memory.rendered_oversize_debt_bytes),
        "render cache exceeded its measured bound: {memory:?}"
    );
    assert!(
        memory.live_block_bytes.saturating_add(hydrated_bytes) < 64 * 1024 * 1024,
        "full block content grew with committed transcript size: {memory:?}"
    );
    if target_bytes >= 50 * 1024 * 1024 {
        assert!(
            memory.evicted_entries > 0,
            "default hydration budget never evicted"
        );
    }
    if target_bytes >= 450 * 1024 * 1024 {
        assert!(
            memory.live_block_bytes.saturating_add(hydrated_bytes) < 50 * 1024 * 1024,
            "500 MiB session retained a transcript-sized full-content copy: {memory:?}"
        );
    }

    let report = serde_json::json!({
        "type": "active_transcript_memory",
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "target_bytes": target_bytes,
        "generated_bytes": generated_bytes,
        "blocks": block_count,
        "save_batches": batch_count,
        "timings_ms": {
            "seed_persist_compact": seeded_ms,
            "first_render": first_render_ms,
            "scroll_20": scroll_20_ms,
            "search": search_ms,
            "next": next_ms,
            "hydration_churn": hydration_churn_ms,
        },
        "block_counts": {
            "live": memory.live_blocks,
            "stored": memory.stored_blocks,
            "hydrated": memory.hydrated_blocks,
        },
        "budgets": {
            "hydrated": memory.hydrated_budget_bytes,
            "descriptor_windows": memory.descriptor_budget_bytes,
            "rendered": memory.rendered_budget_bytes,
        },
        "retained_bytes": {
            "live_blocks": memory.live_block_bytes,
            "live_tool_states": memory.live_tool_state_bytes,
            "hydrated_blocks": memory.hydrated_block_bytes,
            "hydrated_tool_states": memory.hydrated_tool_state_bytes,
            "compact_descriptors": memory.compact_descriptor_bytes,
            "descriptor_windows": memory.descriptor_window_bytes,
            "tool_state_metadata": memory.tool_state_metadata_bytes,
            "origin_hash": memory.origin_hash_bytes,
            "layouts": memory.layout_bytes,
            "source_views": memory.source_view_bytes,
            "active_height_index": memory.height_index_bytes,
            "cached_height_indexes": memory.height_index_cache_bytes,
            "visible_rows": memory.visible_rows_bytes,
            "full_rows": memory.full_rows_bytes,
            "rendered_total": rendered_bytes,
        },
        "pins": {
            "hydrated_bytes": memory.pinned_hydrated_bytes,
            "rendered_bytes": memory.pinned_rendered_bytes,
        },
        "oversize_debt_bytes": {
            "hydrated": memory.hydrated_oversize_debt_bytes,
            "descriptor_windows": memory.descriptor_oversize_debt_bytes,
            "rendered": memory.rendered_oversize_debt_bytes,
        },
        "hydration": {
            "reads": memory.hydration_reads,
            "ranges": memory.hydration_ranges,
            "bytes": memory.hydration_bytes,
            "duration_us": memory.hydration_duration_us,
            "working_set_rereads": working_set_rereads,
        },
        "eviction": {
            "entries": memory.evicted_entries,
            "bytes": memory.evicted_bytes,
        },
        "dematerialization": {
            "entries": memory.dematerialized_entries,
            "bytes": memory.dematerialized_bytes,
        },
        "allocator": {
            "current_bytes_start": process_start.current_bytes,
            "current_bytes_end": process_end.current_bytes,
            "retained_delta_bytes": process_end.current_bytes as i64 - process_start.current_bytes as i64,
            "allocated_bytes": process_delta.bytes_allocated,
            "deallocated_bytes": process_delta.bytes_deallocated,
        },
        "process": {
            "rss_bytes": linux_memory_bytes("VmRSS:"),
            "peak_rss_bytes": linux_memory_bytes("VmHWM:"),
        },
    });
    eprintln!("TRANSCRIPT_ACTIVE_MEMORY_JSON {report}");
    smelt_perf::perf::set_enabled(false);
}
