## Commands

```bash
# build
cargo build

# test (requires `cargo install cargo-nextest` — much faster than `cargo test`)
cargo nextest run --workspace

# format and lint
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings

# coverage (requires `cargo install cargo-llvm-cov`)
cargo llvm-cov nextest --workspace --summary-only

# regenerate Lua API stubs + reference docs (commit the result)
cargo xtask gen-lua-docs
```

Whenever you add a new user-facing feature or change user-facing behavior,
update the README.md and the docs/ folder. Don't document internal
implementation details — only things end users need to know. When you
change the Lua API surface, run `cargo xtask gen-lua-docs` and commit the
regenerated files — CI fails if they're out of sync.

## Conventions

### UTF-8 safety

Byte offsets in this codebase routinely survive across source mutations
(kill-ring source range, vim visual anchor, attachment offsets, undo
snapshots, yank-flash range) and may land mid-char by the time they're
consumed. Raw `&s[a..b]` / `s.drain(a..b)` panic on non-boundaries.

There is exactly **one** module for UTF-8 boundary handling:
`smelt_buffer::text`. Don't introduce snapping logic elsewhere; don't
duplicate these primitives in other crates.

**Reads** — use `smelt_buffer::text::slice(s, range)`. Snaps endpoints
and clamps to `s.len()`; inverted ranges return `""`. Never write
`&source()[a..b]` against a possibly-stale offset.

**Pure-text mutations** — use `smelt_buffer::text::replace_range`,
`text::insert`, `text::insert_str`. Never call `source_mut().drain(...)`
or `source_mut().replace_range(...)` directly.

**Attached-text mutations** (source + `attachment_ids` together) — use
`Buffer::text_mut()` which yields a `smelt_buffer::attached::AttachedTextMut`.
Its `replace_range`, `insert_str`, `insert`, `insert_marker`, `install`,
`set_ids`, `strip_attachments`, `clear` methods are the **only** safe
mutation entry points. They keep marker count and id count in lockstep
(INV-15) and a debug-assert verifies the invariant after every call.

**Boundary math** — `snap`, `prev_char_boundary`, `next_char_boundary`,
`byte_to_cell`, `cell_to_byte`, `char_pos`, `byte_of_char` all live in
`smelt_buffer::text`. `smelt_edit::text` re-exports them. Don't call
`str::is_char_boundary` in feature code — `snap` first instead.

**Writes to `Buffer::source`** — go through `PromptState::install_source`
(or equivalent). It's the single seam that resets cursors, attachments,
undo, and completer state on a buffer-wide swap.
