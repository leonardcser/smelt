## Commands

```bash
# build
cargo build

# test (requires `cargo install cargo-nextest` — much faster than `cargo test`)
cargo nextest run --workspace

# format and lint
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings
```

Whenever you add a new user-facing feature or change user-facing behavior,
update the README.md and the docs/ folder. Don't document internal
implementation details — only things end users need to know.

## Conventions

Any byte range captured against `input.source` (e.g. kill-ring source range,
selection anchors, yank-flash range) must be clamped to current bounds at read
time — `end <= source.len()` and both endpoints on char boundaries. The source
can be cleared, replaced, or shortened between capture and read; slicing with
a stale range panics. Drop the range or fall through; never trust it raw.
