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

fn search_perf_snapshot(label: &str) {
    let snapshot = smelt_perf::perf::snapshot();
    for row in snapshot
        .durations
        .iter()
        .filter(|row| row.label.starts_with("search:transcript"))
    {
        eprintln!(
            "TRANSCRIPT_SEARCH_PERF_DURATION label={} metric={} count={} last_us={} total_us={} p95_us={} max_us={}",
            label, row.label, row.count, row.last_us, row.total_us, row.p95_us, row.max_us
        );
    }
    for row in snapshot
        .values
        .iter()
        .filter(|row| row.label.starts_with("search:transcript"))
    {
        eprintln!(
            "TRANSCRIPT_SEARCH_PERF_VALUE label={} metric={} count={} last={} total={} p95={} max={}",
            label, row.label, row.count, row.last, row.total, row.p95, row.max
        );
    }
}

#[derive(Clone, Copy, Debug)]
struct SearchBenchSample {
    bytes: usize,
    rows: crate::smelt_edit::RowIndex,
    rare_ms: f64,
    common_submit_ms: f64,
    next100_ms: f64,
    after_append_ms: f64,
}

fn run_search_bench_sample(target_bytes: usize, report_perf: bool) -> SearchBenchSample {
    smelt_perf::perf::clear();
    smelt_perf::perf::set_enabled(true);
    let mut app = TestApp::builder().with_vim(true).build();
    app.app.handle_resize(100, 32);
    let bytes = push_search_bench_transcript(&mut app, target_bytes);
    app.render_silent();
    app.app.app_focus = AppFocus::Content;
    app.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
    let win = app.app.transcript_win_mut();
    win.set_vim_enabled(true);
    win.set_vim_mode(VimMode::Normal);
    let rows = transcript_total_rows(&app);

    let rare_start = std::time::Instant::now();
    app.app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        crate::app::search::SearchDirection::Forward,
        "needle-target".into(),
    );
    app.render_silent();
    let rare_ms = elapsed_ms(rare_start.elapsed());
    assert!(transcript_row_cursor_row(&app) > rows / 2);
    if report_perf {
        search_perf_snapshot("rare_cold");
    }

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
    if report_perf {
        search_perf_snapshot("common_hot_next100");
    }

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
    if report_perf {
        search_perf_snapshot("after_append");
    }
    smelt_perf::perf::set_enabled(false);

    SearchBenchSample {
        bytes,
        rows,
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
    let _warmup = run_search_bench_sample(target_bytes, false);
    let mut samples = Vec::with_capacity(runs);
    for run in 0..runs {
        let sample = run_search_bench_sample(target_bytes, true);
        eprintln!(
            "TRANSCRIPT_SEARCH_BENCH_SAMPLE run={} bytes={} rows={} rare_ms={:.3} common_submit_ms={:.3} next100_ms={:.3} after_append_ms={:.3}",
            run + 1,
            sample.bytes,
            sample.rows,
            sample.rare_ms,
            sample.common_submit_ms,
            sample.next100_ms,
            sample.after_append_ms,
        );
        samples.push(sample);
    }
    let rare = NavStats::from(
        &samples
            .iter()
            .map(|sample| sample.rare_ms)
            .collect::<Vec<_>>(),
    );
    let common = NavStats::from(
        &samples
            .iter()
            .map(|sample| sample.common_submit_ms)
            .collect::<Vec<_>>(),
    );
    let next = NavStats::from(
        &samples
            .iter()
            .map(|sample| sample.next100_ms)
            .collect::<Vec<_>>(),
    );
    let after_append = NavStats::from(
        &samples
            .iter()
            .map(|sample| sample.after_append_ms)
            .collect::<Vec<_>>(),
    );
    eprintln!(
        "TRANSCRIPT_SEARCH_BENCH_SUMMARY runs={} bytes={} rows={} rare_mean_ms={:.3} rare_stddev_ms={:.3} common_submit_mean_ms={:.3} common_submit_stddev_ms={:.3} next100_mean_ms={:.3} next100_stddev_ms={:.3} after_append_mean_ms={:.3} after_append_stddev_ms={:.3}",
        samples.len(),
        samples[0].bytes,
        samples[0].rows,
        rare.mean,
        rare.stddev,
        common.mean,
        common.stddev,
        next.mean,
        next.stddev,
        after_append.mean,
        after_append.stddev,
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
        "| navigation/search | rows={} | search={}ms | ctrl-d×20={}ms | ctrl-u×20={}ms | gg={}ms | G={}ms |",
        samples[0].rows,
        search.display(),
        ctrl_d.display(),
        ctrl_u.display(),
        gg.display(),
        g.display(),
    );
}
