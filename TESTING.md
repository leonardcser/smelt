# Testing

Test user-visible behavior and real failure modes at the lowest faithful layer.
Use small, deterministic fixtures. Reproduce bugs through the real workflow when
practical; keep the smallest regression. Do not duplicate contracts across layers.

## Checks

While iterating, run only the crate or tests you touched. Several agents often
share one machine, and a full run keeps every core busy for over a minute,
which slows every other run on the machine. Select tests with a filter on the
workspace build: `-p <crate>` without `--features smelt-tui/harness` resolves
other features and rebuilds the workspace crates a second time.

```bash
set -o pipefail
cargo nextest run --workspace --features smelt-tui/harness -E 'package(<crate>)' 2>&1 | tail -120
cargo nextest run --workspace --features smelt-tui/harness -E 'test(/<pattern>/)' 2>&1 | tail -120
cargo nextest run --workspace --features smelt-tui/harness -E 'binary(storybook_main) & test(<story>)' 2>&1 | tail -120
```

Run the full checks once, before you commit:

```bash
set -o pipefail
cargo fmt --all --check
cargo nextest run --workspace --features smelt-tui/harness 2>&1 | tail -120
cargo clippy --workspace --all-targets --features smelt-tui/harness -- -D warnings 2>&1 | tail -120
```

Each run writes per-test durations to `target/nextest/default/junit.xml`.
`.config/nextest.toml` starts the slowest tests first; update its list when a
test there gets fast or a new one takes seconds. To check that a suspected
flaky test is fixed, repeat it under load with `--stress-count <n>`.

## Ownership

- Unit/component tests: parsing, state, buffers and rendering primitives.
- App regressions: topical `app/harness_tests` modules, not harness internals.
- Storybook: appearance and interactions; review snapshot diffs before accepting.
- Subprocess tests: CLI and real terminal behavior, with an independent parser.
- Protocol tests: `smelt-provider`; engine tests cover orchestration/persistence.
- Fuzz seeds: `fuzz/seeds/<target>/regression/`; see the [fuzz guide](fuzz/README.md).

## Performance

Keep deterministic resource bounds in normal tests. Keep distinct timing workloads
as explicit optimized benchmarks, not commit/push hooks or shared-CI latency gates.
Retain timeout/deadlock checks; remove settled experiments and duplicate metrics.

See [benchmark commands and resource acceptance](docs/transcript-layout-benchmarks.md),
[cross-agent comparisons](benchmarks/memory/README.md), and
[documentation recordings](docs/shots/README.md).
