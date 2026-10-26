## Commands

Pipe through `tail -120` and keep `set -o pipefail` so output stays bounded and failures still surface.

```bash
# build
cargo build
cargo build --profile release-fast --bin smelt   # fast optimized local build
cargo build --profile dist --bin smelt           # distribution build

# test (needs cargo-nextest; the harness feature enables storybook/test helpers)
cargo nextest run --workspace --features smelt-tui/harness
cargo test -p smelt-tui --features harness <filter>   # targeted fallback

# format and lint (CI runs both, fmt first)
cargo fmt
cargo clippy --workspace --all-targets --features smelt-tui/harness -- -D warnings

# coverage gate (needs cargo-llvm-cov; CI requires 80% lines)
CARGO_INCREMENTAL=0 cargo llvm-cov nextest --workspace --features smelt-tui/harness --fail-under-lines 80

# regenerate Lua API stubs and reference docs (commit the result)
cargo xtask gen-lua-docs

# storybook snapshots (run when changing transcript_scene, tool renderers, or permission/transcript UI)
cargo test -p smelt-tui --test storybook_main --features harness app_dialog_permission

# docs terminal replays and the README demo GIF (see docs/shots/README.md)
cd docs/shots && bun install && bun run all
```

Choosing a test layer: see `TESTING.md`. Releases: see `RELEASE.md`.

## Conventions

### Branding

Always write `smelt` in lowercase: product copy, terminal titles, command output,
docs, UI labels, and the start of a sentence.

### Compatibility debt

Mark removable compatibility code with `COMPAT(<id>)` and add the id to
`docs/compat.md` with when and why to remove it. Don't tag normal fallbacks or
provider quirks unless we intend to delete them.

### UTF-8 safety

Byte offsets often outlive the text mutations around them (kill-ring range, vim
visual anchor, attachment offsets, undo snapshots, yank flash) and can land
mid-char. Raw `&s[a..b]` and `s.drain(a..b)` panic on non-boundaries.

All boundary handling lives in `smelt_buffer::text` (re-exported by
`smelt_edit::text`). Don't duplicate it elsewhere, and don't call
`str::is_char_boundary` in feature code; `snap` first.

- **Reads:** `text::slice(s, range)` snaps endpoints, clamps to `s.len()`, and
  returns `""` for inverted ranges.
- **Pure-text mutations:** `text::replace_range`, `text::insert`,
  `text::insert_str`. Never `source_mut().drain(..)` or `.replace_range(..)`.
- **Text with attachments** (source and `attachment_ids` together):
  `Buffer::text_mut()`, which yields `AttachedTextMut`. Its `replace_range`,
  `insert_str`, `insert`, `insert_marker`, `install`, `set_ids`,
  `strip_attachments` and `clear` keep markers and ids in lockstep.
- **Boundary math:** `snap`, `prev_char_boundary`, `next_char_boundary`,
  `byte_to_cell`, `cell_to_byte`, `char_pos`, `byte_of_char`.
- **Swapping the whole buffer source:** `PromptState::install_source`, which
  resets cursors, attachments, undo and completer state.
