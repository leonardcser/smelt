## Commands

```bash
# build
cargo build

# test
cargo test --workspace

# format and lint
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings
```

Whenever you add a new feature or change the current behavior update the docs in
the README.md and the docs/ folder.
