# Window Unification Refactor

Goal: every "viewport into a buffer" surface uses the **same** `ui::Window`
primitive — transcript, prompt, dialog content panels. One mouse handler, one
selection style, one vim-mode source. No `BufferPane`, no parallel
implementations.

Maps to the architecture doc's promise:
> The transcript window and the prompt window are the same kind of thing —
> they only differ in their buffer's `modifiable` flag.

## User complaints this resolves

1. Diff/preview buffers in dialogs lack double/triple-click word/line select.
2. Selection bg color differs from transcript (`DarkGrey` vs `theme::selection_bg()`).
3. Selection behavior differs (no anchored word/line drag, no edge autoscroll).
4. Status bar shows "Insert mode" when a dialog is focused (leaks prompt mode).
5. Clicking outside the diff buffer doesn't blur its cursor.

## Step list

Each step is one atomic commit. Tree green at every commit.
Run `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo nextest run --workspace`.

### ✅ Step 0 — Cherry-pick orthogonal cleanup [commit `82eb4d7`]

`Notification` toast drag-select-and-yank + `selection` field in
`NotificationStyle` + drop dead notification click-to-dismiss path in
`App::handle_mouse`. Independent of the unification.

### ✅ Step 1 — `Window::handle_mouse` + interactive dialog buffer panels [commit `a685eaf`]

- Add `Window::handle_mouse(event, ctx) -> MouseAction` covering click,
  double/triple click word/line yank, drag-extend (anchored to word/line when
  set on Down), mouse-up yank.
- Add `drag_anchor_word` / `drag_anchor_line` state to `Window`.
- `Ui::set_selection_bg(Color)` + `Ui::selection_style() -> Style`.
- `PanelSpec::interactive_content(buf, height)` builder. Lua:
  `{ kind = "content", buf = …, interactive = true }`.
- `Dialog::handle_mouse` routes Down/Drag/Up for interactive Buffer panels
  through their internal `Window::handle_mouse`. `Dialog::cursor` exposes
  the focused panel's Window cursor. Selection overlay painted each frame
  using `theme::selection_bg()` propagated via `DialogConfig::selection_style`.
- Click cadence (1/2/3-click within 400 ms on the same cell, same panel)
  tracked on `Dialog::last_click`. Cross-panel click resets.

Folded in from the stashed work (independent of BufferPane experiment):
- Empty buffer (1 line, empty) → `line_count = 0` so `collapse_when_empty`
  actually hides it.
- Hidden panels suppress separator chrome.
- Regression test `focused_dialog_esc_invokes_dismiss_callback` for the
  dispatch chain.

Resolves complaints 1, 2, 3, 5 for any panel that opts into
`interactive = true`. Built-in confirm preview hasn't been opted in yet
(that's Step 5).

### ✅ Step 2 — Status bar reads the focused buffer Window's vim mode

Fixes complaint 4 (status bar leaks prompt's "Insert" while a dialog is
focused).

Generalize `App::current_vim_mode_label`: drop the `AppFocus::Content/Prompt`
branch, instead ask `Ui` for the focused buffer-bearing Window and read
its mode.

- New `Ui::focused_buffer_window(&self) -> Option<&Window>` that walks the
  compositor's focus chain into the topmost focused dialog, asks the dialog
  for its focused panel's Window (if a buffer panel) and returns it. Falls
  back to the prompt or transcript Window for non-dialog focus.
- When the focused panel is a widget (`OptionList`, `TextInput`) or a
  non-interactive chrome buffer panel: returns `None`, status bar shows no
  mode (matches nvim).

### ✅ Step 3 — Switch transcript path to `Window::handle_mouse`

Pure code-quality: delete the duplicated cursor/selection/drag logic from
`App::handle_mouse`.

Transcript has projection-specific behavior (`full_transcript_display_text`,
`snap_col_to_selectable` for hidden thinking blocks, `copy_display_range` for
display→raw mapping). Keep those in `App` as adapters that build the
`MouseCtx` and translate the returned `Yank(text)`. The cursor / selection /
anchored-drag mechanics move to `Window::handle_mouse` (already there).

Net deletion: ~300 lines from `app/mouse.rs` (`extend_word_anchored_drag`,
`extend_line_anchored_drag`, `select_and_copy_word_in_content`,
`select_and_copy_line_in_content`, `position_content_cursor_from_hit`).

Drag-edge autoscroll stays App-side (frame-tick-driven).

### ⬜ Step 4 — Switch prompt path + drop per-widget selection styles

Prompt has its own quirks: `position_prompt_cursor_from_click` does a
custom char-wrap walk because the prompt buffer is a single multi-line
edit string instead of a `&[String]`. At click time, wrap the prompt
buffer into rows once and feed `Window::handle_mouse`.

Net deletion: `position_prompt_cursor_from_click`, the prompt branches in
`extend_selection_to`, `select_and_copy_word_in_prompt`,
`copy_prompt_selection_on_release`.

Then drop per-widget `selection_style` overrides:
- `TextInput::selection_style` → reads `Ui::selection_style()`.
- `InlineEditField` (if still around after Step 5) → same.
- `NotificationStyle::selection` → already plumbed; check it's the only
  consumer.

One source of truth: theme resolves selection bg/fg, `Ui` slot stores it,
every Window-driven surface reads from there.

### ⬜ Step 5 — Confirm dialog cleanup (the big one)

Separate architectural concern from Window unification. Drops 1000+ lines
across `crates/tui/src/app/dialogs/confirm.rs`,
`crates/tui/src/lua/confirm_ops.rs`, and confirm-specific OptionList flags.

#### 5a — Renderer primitives

Promote confirm-private renderers to general-purpose `smelt.*` modules
that any plugin can use:

```lua
smelt.diff.render_to_buf(buf, { old=, new=, path= })       -- print_inline_diff
smelt.syntax.render_to_buf(buf, { content=, path= })       -- print_syntax_file
smelt.bash.highlight_into(buf, line, col_start, col_end)   -- BashHighlighter
smelt.notebook.render_to_buf(buf, args)                    -- notebook preview
```

Underlying Rust functions stay (security-relevant rendering belongs in
core); confirm-specific wrappers around them go.

#### 5b — Confirm request as data + label policy in Lua

Replace `_build_title_buf`/`_build_summary_buf`/`_build_preview_buf`/
`_option_labels`/`_back_tab` opaque-handle wrappers with a single
`smelt.confirm.requests[handle]` table exposing the request fields
(`tool_name`, `desc`, `summary`, `outside_dir`, `approval_patterns`,
`cwd_label`, `args`). Lua composes the buffers and labels using the
renderer primitives from 5a and standard `smelt.buf.*` helpers.

#### 5c — Panel handles, not index pokes

`smelt.ui.dialog._open(opts)` returns a handle whose `panels` field is a
name-or-index map of typed panel handles:

```lua
local d = smelt.ui.dialog.open({ panels = { options = {...}, reason = {...} } })
d.panels.options:selected_index()
d.panels.reason:text()
d:focus("reason")
```

Replaces every `_focus_panel`, `_options_set_editing`,
`_options_is_editing`, `_focused_panel` confirm_ops shim with panel
methods.

#### 5d — Drop one-off widget flags

- `OptionList::detail_input` → drop. Reason field becomes a regular
  `kind = "input"` panel below options with `hide_when_unfocused = true`,
  Tab focuses it. Same row-spacing as nvim.
- `OptionList::numbered` (`with_index_prefix`) → drop. Caller pre-formats
  labels (`"  1. yes"`) or supplies a prefix function.
- The `interactive` flag stays — it's the unification's primary handle.

#### 5e — Plug `interactive_content` into confirm.lua's preview panel

One-line change in `runtime/lua/smelt/confirm.lua`: the diff/preview panel
becomes `kind = "content", interactive = true`. User immediately gets
double/triple click + theme selection bg + drag-extend in tool approval
dialogs.

Resolution after 5e: every original user complaint addressed end-to-end.

### Net deletion target

After all steps:

- `app/dialogs/confirm.rs` — gone (≈235 lines)
- `app/dialogs/confirm_preview.rs` — kept as-is, but its renderer body
  exposed as `smelt.*` modules (no new wrappers).
- `lua/confirm_ops.rs` — shrinks 250 → ~40 lines (just request snapshot
  + resolve).
- `app/mouse.rs` — sheds ≈300 lines (transcript + prompt mouse handling
  moves to `Window::handle_mouse`).
- `OptionList` detail-field code — gone (≈350 lines if we count the
  stash's additions).
- `BufferPane` — already gone (stashed).

Estimated: ~1000 lines lighter, three "almost-Windows" merged into one
real Window, every buffer surface gets transcript-grade interaction.

## Process notes

- Each step's commit message answers WHY, not WHAT.
- Atomic refactors only — don't ship a step that leaves the tree
  half-migrated.
- Never use `--no-verify` or `--allow-dirty`. Investigate hook failures.
- Stash `stash@{0}` keeps the BufferPane experiment recoverable. Drop
  it once Step 5e ships and we're sure nothing in there is still needed.
