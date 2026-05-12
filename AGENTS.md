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

### UTF-8 safety

Byte offsets in this codebase routinely survive across source mutations
(kill-ring source range, vim visual anchor, attachment offsets, undo
snapshots, yank-flash range) and may land mid-char by the time they're
consumed. Raw `&s[a..b]` / `s.drain(a..b)` panic on non-boundaries.

There is exactly **one** module for UTF-8 boundary handling:
`smelt_buffer::text`. Don't introduce snapping logic elsewhere; don't
duplicate these primitives in other crates.

**Reads** — use `safe_slice(s, range)`. Snaps endpoints and clamps to
`s.len()`; inverted ranges return `""`. Never write `&source()[a..b]`
against a possibly-stale offset.

**Mutations** — use `safe_drain` / `safe_replace_range`. Never call
`source_mut().drain(...)` or `source_mut().replace_range(...)` directly.

**Boundary math** — `snap`, `prev_char_boundary`, `next_char_boundary`,
`byte_to_cell`, `cell_to_byte`, `char_pos`, `byte_of_char` all live in
`smelt_buffer::text`. `smelt_edit::text` re-exports them. Don't call
`str::is_char_boundary` in feature code — `snap` first instead.

**Writes to `Buffer::source`** — go through `PromptState::install_source`
(or equivalent). It's the single seam that resets cursors, attachments,
undo, and completer state on a buffer-wide swap.
