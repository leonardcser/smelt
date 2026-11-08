//! Dev tooling. Invoke as `cargo xtask <command>` (see `.cargo/config.toml`).

#[global_allocator]
static ALLOCATOR: smelt_core::allocator::Allocator = smelt_core::allocator::ALLOCATOR;

mod bench_file_search;
mod bench_session_search;
mod bench_support;
mod bench_transcript_layout;
mod fuzz;
mod gen_file_icons;
mod gen_lua_docs;
mod prepare_release;
mod synth;
mod update_model_catalog;

fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("xtask is at crates/xtask/")
        .to_path_buf()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let cmd = args.next();
    match cmd.as_deref() {
        Some("bench-diff") => {
            for (package, test_filter, features) in [
                ("smelt-transcript", "diff_index_benchmark", &[][..]),
                ("smelt-tui", "diff_viewer_benchmark", &["harness"][..]),
                ("smelt-tui", "diff_loading_benchmark", &["harness"][..]),
                ("smelt-tui", "diff_interaction_benchmark", &["harness"][..]),
            ] {
                bench_support::run_cargo_test_benchmark(bench_support::CargoTestBenchmark {
                    package,
                    test_filter,
                    features,
                    release: true,
                    env: Vec::new(),
                    bench_name: test_filter,
                });
            }
        }
        Some("bench-search") => {
            bench_support::run_cargo_test_benchmark(bench_support::CargoTestBenchmark {
                package: "smelt-tui",
                test_filter: "viewer_search_benchmark",
                features: &["harness"],
                release: true,
                env: Vec::new(),
                bench_name: "viewer_search_benchmark",
            });
        }
        Some("bench-files") => {
            for test_filter in ["file_finder_benchmark", "code_search_benchmark"] {
                bench_support::run_cargo_test_benchmark(bench_support::CargoTestBenchmark {
                    package: "smelt-tui",
                    test_filter,
                    features: &["harness"],
                    release: true,
                    env: Vec::new(),
                    bench_name: test_filter,
                });
            }
        }
        Some("bench-file-search") => bench_file_search::run(args.collect()),
        Some("bench-session-search") => bench_session_search::run(args.collect()),
        Some("bench-transcript-layout") => bench_transcript_layout::run(args.collect()),
        Some("gen-file-icons") => gen_file_icons::run(args.collect()),
        Some("gen-lua-docs") => gen_lua_docs::run(),
        Some("prepare-release") => prepare_release::run(args.collect()),
        Some("synth") => synth::run(),
        Some("update-model-catalog") => update_model_catalog::run(args.collect()),
        Some("fuzz") => fuzz::run(args.collect()),
        Some(other) => {
            eprintln!("xtask: unknown command `{other}`");
            print_usage();
            std::process::exit(2);
        }
        None => {
            print_usage();
            std::process::exit(2);
        }
    }
}

fn print_usage() {
    eprintln!("usage: cargo xtask <command> [args]");
    eprintln!();
    eprintln!("commands:");
    eprintln!("  bench-diff                            benchmark indexed diffs and million-line overlay frames");
    eprintln!(
        "  bench-search                          benchmark typing a search in a large viewer"
    );
    eprintln!("  bench-files                           benchmark file and code search previews");
    eprintln!(
        "  bench-file-search [--runs N] [--entries N] [--queries CSV] benchmark file fuzzy search"
    );
    eprintln!(
        "  bench-session-search --state-dir PATH [--session ID] [--runs N] [--allow-debug] benchmark saved transcript search"
    );
    eprintln!(
        "  bench-transcript-layout [--runs N] [--workloads CSV] [--scale-500mb] run transcript layout benches"
    );
    eprintln!("  gen-file-icons [DEVICONS_DIR]          regenerate nvim-web-devicons registry");
    eprintln!("  gen-lua-docs                          regenerate Lua API stubs + reference docs");
    eprintln!("  prepare-release VERSION               materialize a tag version in this checkout");
    eprintln!(
        "  synth                                 generate a synthetic session for perf testing"
    );
    eprintln!("  update-model-catalog [API_JSON]       refresh the embedded models.dev snapshot");
    eprintln!("  fuzz run <target> [--fork N] [--cmin] fuzz a target until crash or Ctrl-C");
    eprintln!(
        "  fuzz triage <target> <artifact>       shrink a crash and print the minimal scenario"
    );
    eprintln!("  fuzz replay-regression                replay every committed regression seed");
    eprintln!("  fuzz coverage-snapshot [target...]    per-target source coverage snapshot");
}
