## Commands

```bash
# build
cargo build

# fast optimized local build
cargo build --profile release-fast --bin smelt

# distribution release build
cargo build --profile dist --bin smelt

# test (requires `cargo install cargo-nextest` — much faster and quieter than `cargo test`; enables smelt-tui's harness feature for storybook/test helpers)
set -o pipefail; cargo nextest run --workspace --features smelt-tui/harness 2>&1 | tail -120

# targeted cargo test fallback: keep output bounded; rerun narrower tests if tail omits context
set -o pipefail; cargo test -p smelt-tui --features harness double_esc 2>&1 | tail -120

# format and lint
set -o pipefail; cargo fmt && cargo clippy --workspace --all-targets --features smelt-tui/harness -- -D warnings 2>&1 | tail -120

# coverage / CI-equivalent test gate (requires `cargo install cargo-llvm-cov`; enables smelt-tui's harness feature for storybook/test helpers)
set -o pipefail; cargo llvm-cov nextest --workspace --features smelt-tui/harness --fail-under-lines 80 2>&1 | tail -120

# quick coverage summary (does not enforce CI's coverage floor)
set -o pipefail; cargo llvm-cov nextest --workspace --features smelt-tui/harness --summary-only 2>&1 | tail -120

# regenerate Lua API stubs + reference docs (commit the result)
set -o pipefail; cargo xtask gen-lua-docs 2>&1 | tail -120
```

## Conventions

### Compatibility debt

Mark removable compatibility code with `COMPAT(<id>)` and add the id to
`docs/compat.md` with when/why to remove it. Don't tag normal fallbacks or
provider quirks unless we intend to delete them.

### UTF-8 safety

Byte offsets in this codebase routinely survive across source mutations
(kill-ring source range, vim visual anchor, attachment offsets, undo snapshots,
yank-flash range) and may land mid-char by the time they're consumed. Raw
`&s[a..b]` / `s.drain(a..b)` panic on non-boundaries.

There is exactly **one** module for UTF-8 boundary handling:
`smelt_buffer::text`. Don't introduce snapping logic elsewhere; don't duplicate
these primitives in other crates.

**Reads** — use `smelt_buffer::text::slice(s, range)`. Snaps endpoints and
clamps to `s.len()`; inverted ranges return `""`. Never write `&source()[a..b]`
against a possibly-stale offset.

**Pure-text mutations** — use `smelt_buffer::text::replace_range`,
`text::insert`, `text::insert_str`. Never call `source_mut().drain(...)` or
`source_mut().replace_range(...)` directly.

**Attached-text mutations** (source + `attachment_ids` together) — use
`Buffer::text_mut()` which yields a `smelt_buffer::attached::AttachedTextMut`.
Its `replace_range`, `insert_str`, `insert`, `insert_marker`, `install`,
`set_ids`, `strip_attachments`, `clear` methods are the **only** safe mutation
entry points. They keep marker count and id count in lockstep and a debug-assert
verifies the invariant after every call.

**Boundary math** — `snap`, `prev_char_boundary`, `next_char_boundary`,
`byte_to_cell`, `cell_to_byte`, `char_pos`, `byte_of_char` all live in
`smelt_buffer::text`. `smelt_edit::text` re-exports them. Don't call
`str::is_char_boundary` in feature code — `snap` first instead.

**Writes to `Buffer::source`** — go through `PromptState::install_source` (or
equivalent). It's the single seam that resets cursors, attachments, undo, and
completer state on a buffer-wide swap.
