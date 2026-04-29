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

## Refactor docs

This workspace is in the middle of a multi-phase architectural refactor.
Read `refactor/README.md` first — it indexes the rest and carries the
operating rules.

@refactor/README.md
@refactor/REFACTOR.md
@refactor/ARCHITECTURE.md
@refactor/tui-ui-architecture-target.puml
@refactor/INVENTORY.md
@refactor/FEATURES.md
@refactor/DECISIONS.md
@refactor/TRACE.md
@refactor/TESTING.md
@refactor/P1.md
