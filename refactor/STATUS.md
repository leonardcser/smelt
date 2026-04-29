# Status

Where we are right now. Updated at session end.

For the entry point and meta-rules, read `README.md` first.

## Where we are

**Phase:** P1.0 theme registry landed end-to-end. Atomic state
collapsed into `ui::Theme`; `crate::theme::*` is now a small
smelt-specific init module (`populate_ui_theme` + `detect_background`
+ `PRESETS`).

**Tree:** green. `cargo nextest run --workspace` — 914 passed (908
from prior boundary + 6 new tests for atomic-on-Theme + helper-
function direct coverage). `cargo nextest run --test scenarios` — 6
baseline scenarios green.

**Last update:** 2026-04-29. P1.0 theme registry landing across 12
commits (`decb0ab`..`e489a79`):

- `decb0ab` — `ui::Theme` registry type (HashMap groups + links).
- `177ac4c` — plumbed through `DrawContext`; `Ui` owns it.
- `bb9cc63` — `populate_ui_theme()` mirrors host constants.
- `9bf1912` — first call site batch: render_loop, events,
  status_bar, ui_ops.
- `31cfb56` — renamed `crate::theme::Theme` snapshot → `Snapshot`.
- `d92a715` — status separator color + notification error_label.
- `da75d3a` — re-populate registry each frame so Lua-driven mutations
  propagate without a separate notification path.
- `387f4d2` — replaced `crate::theme::Snapshot` with `&ui::Theme` in
  the entire render pipeline; deleted the snapshot type.
- `1786716` — added `ColorRole::Agent / Success / ErrorMsg`; migrated
  `transcript_present/*` renderers off the const colors.
- `16beb71` — threaded `&ui::Theme` through `compute_prompt` /
  `reasoning_color` / `WindowView::draw_scrollbar`; migrated
  `confirm_preview` notebook title; added `ColorRole::Apply / Plan /
  Exec / Heading / ReasonLow / ReasonMed / ReasonHigh / ReasonMax`.
- `7aadcd2` — threaded `&ui::Theme` through
  `WindowView::set_soft_cursor` (last renderer-side `theme::is_light`
  caller).
- `e489a79` — collapsed atomics into `ui::Theme`. `accent`, `slug`,
  `is_light` live as fields with proper accessors; the per-frame
  `populate_ui_theme()` reads them and rewrites the `Smelt*` groups.
  Lua API closures access theme via `with_app(|app| app.ui.theme())`
  — no global state on either side. Inline ANSI helpers in
  `headless.rs` use literal `Color::Red` / `Color::AnsiValue(77)`
  rather than const aliases. Syntect's light/dark hint moved into
  `content/highlight/mod.rs` as a self-contained mirror updated by
  `populate_ui_theme()` (avoids threading `&Theme` through 14 syntax
  call sites for one branch).

`crate::theme::*` is now narrow:
- `populate_ui_theme(&mut Theme)` — initializes `Smelt*` highlight
  groups from `theme.is_light()` + `theme.accent()` + `theme.slug()`.
- `detect_background(&mut Theme)` — OS-level OSC 11 / `$COLORFGBG`
  probe; sets `theme.set_light(...)` if successful.
- `PRESETS` — preset accent picker list (12 colors).
- `preset_by_name(&str) -> Option<u8>` — Lua API helper.
- `DEFAULT_ACCENT` re-export from `ui::theme::DEFAULT_ACCENT`.

P0 closed: 4 of 9 deletions shipped (orthogonal); 5 structural
deletions still deferred to later P1 sub-phases (paired with
replacements):
- `BufferView` deletion paired with `Window::render(buf, grid)` —
  P1.d.
- `PanelWidget` trait + dialog.rs panel multiplexing — P1.c.
- `Component` trait + remaining `WidgetEvent` — P1.d.
- `Placement` enum + `add_layer`/`register_split` plumbing — P1.b +
  P1.c.

**Note for next session:** puml + SVG are in sync. If the puml is
edited, regenerate via `plantuml -tsvg
refactor/tui-ui-architecture-target.puml`. Run `refactor/check.sh`
before declaring anything done.

## What's next

Pick a P1 sub-phase to tackle. Each is a substantial rewrite:

1. **P1.a — `Buffer` rewrite** (extmarks + namespaces + `Buffer::attach`
   parser hooks). Mirrors `nvim_buf_set_extmark`. This is the
   foundation everything else rides on.
2. **P1.b — `LayoutTree`**. `Vbox`/`Hbox`/`Leaf(WinId)` with
   constraints; container chrome (border/title/separator/gap).
3. **P1.c — `Overlay` replacing `Float`**. Single overlay primitive
   with anchors; deletes `PanelWidget` + `dialog.rs` multiplexing.
4. **P1.d — `Window` as only interactive unit**. Cursor/scroll/
   selection/keymap on one struct; vim/completer state machines
   decompose; deletes `Component` trait + `BufferView`.

P1.a is the natural starting point — buffers underlie everything.

Recently shipped: theme registry + plumbing + atomic-on-Theme
collapse + 42 call site migrations + Snapshot elimination + ColorRole
expansion.
P0 orthogonal deletions
(`selection_style`/`set_selection_bg` shim, `handle_mouse_with_lua` +
`classify_widget_action`, `MouseAction::Yank`/`WidgetEvent::Yank`,
`BufferList`). `TESTING.md` (three-layer testing strategy). Test
harness + 5 baseline scenarios (`plain_turn`, `thinking_then_text`,
`incomplete_stream`, `provider_auth_error`, `streaming_concat_across_deltas`).
`TRACE.md` vertical-slice walk-through.

## Open questions / blocked

(Rows from `INVENTORY.md` that need a decision before their phase
begins. Move resolved items down to a "decisions" log here or in the
relevant `P<n>.md`.)

- **`commands.lua` shape** — P4.e. One master file vs one-per-command in
  `plugins/`. Decide while coding P4.e.
- **`render_ops.rs` per-language split** — P3.b. One `parse.rs` binding,
  or per-language (`diff.rs`/`syntax.rs`)? Decide while coding P3.b.
- **`prompt_picker.lua`** — P4.a. Merge into `widgets/picker.lua` or stay
  separate as `widgets/prompt_picker.lua`? Decide while coding P4.a.
- **`predict.lua` location** — P4. Stay in `plugins/` (it's a hook), or
  move under a new `hooks/` dir? Decide while coding P4.
- **`plan_mode.lua` split** — P4/P5. Tool half → `tools/exit_plan_mode.lua`,
  hook half → `modes.lua` or `plugins/`. Confirm split shape when P4/P5
  begin.

None of these block the start of P0 or P1.

## Recently landed

(Phases as they complete. Newest first. One line each, link to the
P-log.)

- **P0 — Clear the deck (orthogonal half).** 4 of 9 deletions
  shipped; 5 structural deletions deferred to P1.0. Tree green; 901
  workspace tests + 6 baseline scenarios passing. See `P0.md`.

## How to keep this file current

- At session end: update "Where we are", "What's next", "Open
  questions" — even one line is enough.
- When a phase boundary lands: rotate the line into "Recently landed",
  reset "What's next."
- When a decision is made: log it in the active `P<n>.md`; if it
  changes scope, also update `REFACTOR.md` in the same commit.

(For meta-rules and the cold-start checklist, see `README.md`.)
