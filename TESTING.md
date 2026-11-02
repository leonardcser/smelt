# Testing

Test behavior that matters to users or protects a clear failure mode. Prefer
small fixtures and deterministic assertions. Do not add tests only for coverage,
duplicate the same contract across layers, or snapshot implementation details.

## Choose a layer

Use the lowest layer that reproduces the behavior faithfully:

| Layer | Use for |
| --- | --- |
| Unit | Parsing, UTF-8 helpers, permissions, pure state transitions |
| Component | Real buffers, windows, editing, layout and rendering primitives |
| App | Lua, focus, input and session interactions through `TestApp` |
| Storybook | User-visible layout, styles and interaction sequences |
| Subprocess | CLI, config, providers and terminal ownership |
| Fuzz | Large or history-dependent state spaces |

Keep app regressions in topical `app/harness_tests` modules, not the harness
implementation. Keep business-logic assertions out of visual snapshots.
Commit fuzz regression seeds under `fuzz/seeds/<target>/regression/`.

## Run checks

```bash
set -o pipefail
cargo fmt --all --check
cargo nextest run --workspace --features smelt-tui/harness 2>&1 | tail -120
cargo clippy --workspace --all-targets --features smelt-tui/harness -- -D warnings 2>&1 | tail -120
```

For focused UI work:

```bash
set -o pipefail
cargo test -p smelt-tui --features harness <filter> 2>&1 | tail -120
cargo test -p smelt-tui --test storybook_main --features harness <story> 2>&1 | tail -120
```

Review snapshot diffs before accepting updates. For a bug fix, first reproduce
the failure as a user would, then keep the smallest regression that catches it.
