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

fn navigation_bench_runs() -> usize {
    std::env::var("SMELT_TRANSCRIPT_BENCH_RUNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|runs| *runs > 0)
        .unwrap_or(3)
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
    std::env::var("SMELT_TRANSCRIPT_BENCH_SEARCH_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|bytes| *bytes > 0)
        .unwrap_or(50 * 1024 * 1024)
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
    rare_ms: f64,
    common_submit_ms: f64,
    next100_ms: f64,
    after_append_ms: f64,
}

fn search_bench_metric_values(sample: &SearchBenchSample) -> [(&'static str, f64); 12] {
    [
        ("width_resize", sample.width_resize_ms),
        ("height_resize", sample.height_resize_ms),
        ("theme_color", sample.theme_color_ms),
        ("copy_mid", sample.copy_mid_ms),
        ("nav_ctrl_d20", sample.nav_ctrl_d20_ms),
        ("nav_ctrl_u20", sample.nav_ctrl_u20_ms),
        ("nav_gg", sample.nav_gg_ms),
        ("nav_G", sample.nav_g_ms),
        ("rare", sample.rare_ms),
        ("common_submit", sample.common_submit_ms),
        ("next100", sample.next100_ms),
        ("after_append", sample.after_append_ms),
    ]
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

fn run_search_bench_sample(target_bytes: usize, report_perf: bool) -> SearchBenchSample {
    smelt_perf::perf::clear();
    smelt_perf::perf::set_enabled(true);
    let mut app = TestApp::builder().with_vim(true).build();
    app.app.handle_resize(100, 32);
    let bytes = push_search_bench_transcript(&mut app, target_bytes);
    app.render_silent();
    app.app.save_session();
    app.app.flush_persist();
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
        rare_ms,
        common_submit_ms,
        next100_ms,
        after_append_ms,
    }
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
}

impl HotPathCounters {
    fn from(snapshot: &smelt_perf::perf::Snapshot) -> Self {
        Self {
            history_suffix_rows: perf_value_max(
                snapshot,
                "store:session:dirty_suffix_history_rows",
            ),
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
        }
    }
}

#[derive(Clone, Debug)]
struct HotPathSample {
    operation: &'static str,
    history_len: usize,
    ms: f64,
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

fn hot_path_session_id(label: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "transcript-hot-path-{label}-{}-{counter}-{nanos}",
        std::process::id()
    )
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

fn hot_path_history_item(idx: usize) -> protocol::HistoryItem {
    if idx.is_multiple_of(2) {
        hot_path_user(&format!("hot path old user {idx}"))
    } else {
        hot_path_assistant(&format!("hot path old assistant {idx}"))
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
    session.history = (0..history_len).map(hot_path_history_item).collect();
    if let Some(first_live_index) = checkpoint_first_live {
        session.checkpoint = Some(smelt_core::ContextCheckpoint {
            kind: "benchmark".into(),
            summary: "checkpointed benchmark prefix".into(),
            first_live_index: first_live_index.min(history_len),
            created_at_ms: smelt_core::session::now_ms(),
            tokens_before: Some(10_000),
            tokens_after_estimate: Some(1_000),
            pre_checkpoint_context_tokens: None,
            pre_checkpoint_context_history_len: None,
        });
    }

    app.app.load_session(session);
    app.app.restore_screen();
    app.app.persisted_store_ready = false;
    app.app.save_session();
    app.app.flush_persist();
    app
}

fn assert_no_full_hot_path_reads(snapshot: &smelt_perf::perf::Snapshot, operation: &str) {
    for metric in [
        "store:history:read_all",
        "store:history:read_all_rows",
        "store:session:load_full_snapshot",
        "store:session:full_snapshot_rows_read",
        "store:transcript:search_blob_full",
        "store:transcript:read_descriptors_full",
        "store:transcript:descriptors_full_loaded",
        "transcript:build_from_session:history_items",
        "transcript:render_plan:fingerprint",
        "persist:write:blobs",
    ] {
        let value = perf_value_max(snapshot, metric);
        assert_eq!(
            value, 0,
            "{operation} recorded {metric}={value}, expected no full-session hot-path work"
        );
    }
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
    let start = std::time::Instant::now();
    body();
    let ms = elapsed_ms(start.elapsed());
    let snapshot = smelt_perf::perf::snapshot();
    smelt_perf::perf::set_enabled(false);
    let sample = HotPathSample {
        operation,
        history_len,
        ms,
        counters: HotPathCounters::from(&snapshot),
    };
    (sample, snapshot)
}

fn read_provider_history_source(
    source: protocol::ModelHistorySource,
    session_dir: &std::path::Path,
) -> Vec<protocol::HistoryItem> {
    match source {
        protocol::ModelHistorySource::Items(items) => items,
        protocol::ModelHistorySource::Store {
            prefix,
            first_live_index,
            end_index,
        } => {
            smelt_perf::perf::record_value("engine:model_history:source_store", 1);
            smelt_perf::perf::record_value(
                "engine:model_history:first_live_index",
                first_live_index as u64,
            );
            smelt_perf::perf::record_value("engine:model_history:end_index", end_index as u64);
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
        "persist:write:history_items",
        0,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "persist:write:descriptor_records",
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
        "persist:write:history_items",
        1,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:session:dirty_suffix_history_rows",
        1,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:history:dirty_suffix_rows",
        1,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:transcript:dirty_descriptor_suffix_rows",
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
                first_index: history_len,
                items: vec![item],
            });
        app.app.flush_persist();
    });
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "persist:write:history_items",
        1,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:session:dirty_suffix_history_rows",
        1,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:history:dirty_suffix_rows",
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
                first_changed_index: history_len,
                history: None,
                meta: Some(meta),
            });
        app.app.flush_persist();
    });
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "persist:write:history_items",
        0,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:session:dirty_suffix_history_rows",
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
    assert_eq!(
        perf_duration_max(&snapshot, "store:session:save_history_suffix_transaction"),
        0,
        "{} used the history-suffix save path on metadata-only turn completion",
        sample.operation
    );
    assert_eq!(
        perf_duration_max(&snapshot, "store:transcript:replace_descriptor_suffix"),
        0,
        "{} touched transcript descriptors on metadata-only turn completion",
        sample.operation
    );
    assert_eq!(
        perf_value_max(&snapshot, "persist:write:metadata_only"),
        1,
        "{} did not use the metadata-only persist path",
        sample.operation
    );
    assert_no_full_hot_path_reads(&snapshot, sample.operation);
    assert_cached_persist_db(&snapshot, sample.operation);
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
        "persist:write:history_items",
        0,
    );
    assert_hot_path_at_most(
        &snapshot,
        sample.operation,
        "store:session:dirty_suffix_history_rows",
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

fn print_hot_path_perf(operation: &str, snapshot: &smelt_perf::perf::Snapshot) {
    for row in snapshot.durations.iter().filter(|row| {
        [
            "session:", "persist:", "store:", "engine:", "tui:", "lua:", "bench:",
        ]
        .iter()
        .any(|prefix| row.label.starts_with(prefix))
    }) {
        eprintln!(
            "TRANSCRIPT_HOT_PATH_PERF_DURATION operation={} metric={} count={} last_us={} total_us={} p95_us={} max_us={}",
            operation, row.label, row.count, row.last_us, row.total_us, row.p95_us, row.max_us
        );
    }
    for row in snapshot.values.iter().filter(|row| {
        [
            "session:", "persist:", "store:", "engine:", "tui:", "lua:", "bench:",
        ]
        .iter()
        .any(|prefix| row.label.starts_with(prefix))
    }) {
        eprintln!(
            "TRANSCRIPT_HOT_PATH_PERF_VALUE operation={} metric={} count={} last={} total={} p95={} max={}",
            operation, row.label, row.count, row.last, row.total, row.p95, row.max
        );
    }
}

fn print_hot_path_sample(run: usize, sample: &HotPathSample) {
    let c = sample.counters;
    eprintln!(
        "TRANSCRIPT_HOT_PATH_BENCH_SAMPLE run={} operation={} history_len={} ms={:.3} history_suffix_rows={} history_inserted={} history_deleted={} descriptor_suffix_rows={} descriptor_inserted={} descriptor_deleted={} read_range_rows={} cached_read_write_db={}",
        run,
        sample.operation,
        sample.history_len,
        sample.ms,
        c.history_suffix_rows,
        c.history_inserted,
        c.history_deleted,
        c.descriptor_suffix_rows,
        c.descriptor_inserted,
        c.descriptor_deleted,
        c.read_range_rows,
        c.cached_read_write_db,
    );
    eprintln!(
        "TRANSCRIPT_HOT_PATH_BENCH_JSON {{\"type\":\"hot_path_sample\",\"run\":{},\"operation\":\"{}\",\"history_len\":{},\"ms\":{:.3},\"history_suffix_rows\":{},\"history_inserted\":{},\"history_deleted\":{},\"descriptor_suffix_rows\":{},\"descriptor_inserted\":{},\"descriptor_deleted\":{},\"read_range_rows\":{},\"cached_read_write_db\":{}}}",
        run,
        sample.operation,
        sample.history_len,
        sample.ms,
        c.history_suffix_rows,
        c.history_inserted,
        c.history_deleted,
        c.descriptor_suffix_rows,
        c.descriptor_inserted,
        c.descriptor_deleted,
        c.read_range_rows,
        c.cached_read_write_db,
    );
}

#[test]
#[ignore = "manual transcript save/request hot-path benchmark suite; prefer `cargo xtask bench-transcript-layout`"]
fn transcript_layout_hot_path_benchmark_suite() {
    if !hot_path_enabled() {
        eprintln!("TRANSCRIPT_HOT_PATH_BENCH_SKIPPED set SMELT_TRANSCRIPT_HOT_PATH=1 to run");
        return;
    }

    let runs = navigation_bench_runs();
    let history_len = hot_path_history_len();
    let mut samples = Vec::new();
    for run in 1..=runs {
        for (sample, snapshot) in [
            run_noop_save_hot_path(history_len),
            run_request_append_hot_path(history_len),
            run_history_appended_hot_path(history_len),
            run_turn_complete_hot_path(history_len),
            run_rewind_delete_hot_path(history_len),
            run_provider_history_hot_path(history_len),
        ] {
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
    ] {
        let operation_samples = samples
            .iter()
            .filter(|sample| sample.operation == operation)
            .map(|sample| sample.ms)
            .collect::<Vec<_>>();
        let stats = NavStats::from(&operation_samples);
        eprintln!(
            "TRANSCRIPT_HOT_PATH_BENCH_SUMMARY operation={} runs={} history_len={} mean_ms={:.3} stddev_ms={:.3}",
            operation,
            operation_samples.len(),
            history_len,
            stats.mean,
            stats.stddev,
        );
        eprintln!(
            "TRANSCRIPT_HOT_PATH_BENCH_SUMMARY_JSON {{\"type\":\"hot_path_summary\",\"operation\":\"{}\",\"runs\":{},\"history_len\":{},\"mean_ms\":{:.3},\"stddev_ms\":{:.3}}}",
            operation,
            operation_samples.len(),
            history_len,
            stats.mean,
            stats.stddev,
        );
    }
}
