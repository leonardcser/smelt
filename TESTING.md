# Testing

Test user-visible behavior and real failure modes at the lowest faithful layer.
Use small, deterministic fixtures. Reproduce bugs through the real workflow when
practical; keep the smallest regression. Do not duplicate contracts across layers.

## Checks

```bash
set -o pipefail
cargo fmt --all --check
cargo nextest run --workspace --features smelt-tui/harness 2>&1 | tail -120
cargo clippy --workspace --all-targets --features smelt-tui/harness -- -D warnings 2>&1 | tail -120
```

Focused UI checks:

```bash
set -o pipefail
cargo test -p smelt-tui --features harness <filter> 2>&1 | tail -120
cargo test -p smelt-tui --test storybook_main --features harness <story> 2>&1 | tail -120
```

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
