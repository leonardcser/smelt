use super::*;

#[derive(Clone, Copy, Debug)]
enum Workload {
    ModelContext,
    Archived,
    Snapshots,
}

fn fixture(root: &Path, count: usize, workload: Workload) -> String {
    let mut session = smelt_core::session::Session::new(1, root.to_owned());
    let mut transcript = smelt_transcript::content::transcript::Transcript::new();
    let identity = smelt_core::session::ContextTokenIdentity {
        model: Some("local/test-model".into()),
        api_base: Some("http://127.0.0.1:9/v1".into()),
        provider_type: Some("openai-compatible".into()),
    };
    for index in 0..count {
        let text = if index + 1 == count {
            "quit-scaling-ready".to_owned()
        } else {
            format!("record {index}: {}", "payload ".repeat(64))
        };
        session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text(
                text.clone(),
            )));
        transcript.push(smelt_core::Block::Text {
            content: text.into(),
        });
        if matches!(workload, Workload::Snapshots) {
            session.turn_metas.push((
                index + 1,
                protocol::TurnMeta {
                    elapsed_ms: 1000,
                    avg_tps: Some(50.0),
                    display_tps: Some(50.0),
                    interrupted: false,
                },
            ));
            session.context_snapshots.push((
                index + 1,
                smelt_core::session::ContextSnapshot {
                    context_tokens: Some(1000),
                    context_tokens_history_len: Some(index + 1),
                    context_token_identity: Some(identity.clone()),
                    display_context_tokens: Some(1000),
                    display_context_token_identity: Some(identity.clone()),
                    checkpoint: None,
                },
            ));
        }
    }
    if !matches!(workload, Workload::ModelContext) {
        session.checkpoint = Some(smelt_core::ContextCheckpoint {
            summary: "bounded logical model context".into(),
            first_live_index: count.saturating_sub(4),
            ..Default::default()
        });
    }
    let records = transcript
        .history
        .block_records()
        .into_iter()
        .enumerate()
        .map(|(index, record)| {
            smelt_transcript::transcript_model::transcript_block_row_with_block_idx(
                index,
                index as u64,
                &record,
            )
        })
        .collect::<Result<Vec<_>, _>>()
        .expect("encode quit fixture transcript");
    commit_session_fixture(root, &session, records);
    session.id
}

#[test]
#[ignore = "manual PTY quit scaling benchmark; see docs/transcript-layout-benchmarks.md"]
fn interactive_quit_scaling_benchmark() {
    let sizes =
        std::env::var("SMELT_QUIT_BENCH_SIZES").unwrap_or_else(|_| "4096,65536,262144".into());
    let sizes: Vec<usize> = sizes
        .split(',')
        .map(|size| size.trim().parse().expect("SMELT_QUIT_BENCH_SIZES"))
        .collect();
    assert!(
        !sizes.is_empty()
            && sizes.iter().all(|&size| size > 0)
            && sizes.windows(2).all(|pair| pair[0] < pair[1]),
        "sizes must be positive and strictly increasing"
    );
    let runs: usize = std::env::var("SMELT_LIFECYCLE_BENCH_RUNS")
        .unwrap_or_else(|_| "5".into())
        .parse()
        .expect("SMELT_LIFECYCLE_BENCH_RUNS");
    assert!(runs > 0, "runs must be positive");
    let binary = std::env::var_os("SMELT_LIFECYCLE_BENCH_TARGET")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_smelt").into());
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("init.lua");
    std::fs::write(
        &config,
        r#"smelt.settings.autoupgrade = "off"
smelt.settings.auto_reload = false
smelt.provider.register("local", {
  type = "openai-compatible",
  api_base = "http://127.0.0.1:9/v1",
  models = { "test-model" },
})
"#,
    )
    .unwrap();
    for workload in [
        Workload::ModelContext,
        Workload::Archived,
        Workload::Snapshots,
    ] {
        for &count in &sizes {
            let fixture_root = root.path().join("fixture");
            let session_id = fixture(&fixture_root, count, workload);
            let benchmark = LifecycleBenchmark {
                binary: &binary,
                fixture: Some(&fixture_root),
                session_id: Some(&session_id),
                config: &config,
                first_frame_marker: b"local/test-model",
                ready_marker: b"quit-scaling-ready",
                timeout: Duration::from_secs(60),
            };
            let mut samples = Vec::new();
            for run in 0..=runs {
                let run_root = root.path().join(format!("run-{run:03}"));
                let sample = benchmark.run_sample(&run_root);
                assert!(
                    sample.terminal_restored.is_some(),
                    "terminal was not restored"
                );
                std::fs::remove_dir_all(run_root).unwrap();
                if run > 0 {
                    samples.push(sample);
                }
            }
            println!("QUIT_SCALING workload={workload:?} records={count}");
            print_duration_summary("ready", samples.iter().map(|sample| sample.ready));
            print_duration_summary("shutdown", samples.iter().map(|sample| sample.shutdown));
            print_duration_summary(
                "terminal_restored",
                samples.iter().filter_map(|sample| sample.terminal_restored),
            );
            print_sample_summary(
                "peak_rss",
                "kib",
                samples
                    .iter()
                    .map(|sample| sample.peak_rss_kib as f64)
                    .collect(),
            );
            std::fs::remove_dir_all(fixture_root).unwrap();
        }
    }
}
