# Status

Where we are right now. Updated at session end.

For the entry point and meta-rules, read `README.md` first.

## Where we are

**Phase:** P1.0 in progress. Theme registry foundation landed; call
site migration pending.

**Tree:** green. `cargo nextest run --workspace` — 908 passed (901
from P0 boundary + 7 new theme registry tests). `cargo nextest run
--test scenarios` — 6 baseline scenarios green.

**Last update:** 2026-04-29. P1.0 opened with the theme registry across
8 commits (`decb0ab`..`da75d3a`):

- `decb0ab` — `ui::Theme` registry type (HashMap groups + links).
- `177ac4c` — plumbed through `DrawContext`; `Ui` owns it.
- `bb9cc63` — `populate_ui_theme()` mirrors host constants.
- `2e5adfa` — STATUS / INVENTORY documentation.
- `9bf1912` — first call site batch: render_loop, events,
  status_bar, ui_ops.
- `31cfb56` — renamed `crate::theme::Theme` snapshot → `Snapshot` to
  free the name for `ui::Theme`.
- `d92a715` — status separator color + notification error_label.
- `da75d3a` — re-populate registry each frame so Lua-driven mutations
  (`smelt.theme.set('accent', …)`) propagate without a separate
  notification path.

Migration count: 16 of 50 `crate::theme::*` call sites converted
(34 remain). The registry and constants module run in parallel —
populated with the same values each frame — so each call site can
migrate independently.

Remaining call sites split into:
1. **Snapshot users** (~6): `format.rs`, `transcript.rs`,
   `content/to_buffer.rs`, `content/transcript_buf.rs`,
   `content/context.rs`, `app/dialogs/confirm_preview.rs`. Use
   `crate::theme::Snapshot` for per-render color capture; migrate to
   `&ui::Theme` once `Snapshot` is replaced.
2. **Renderer constants** (~7): `app/transcript_present/*` use
   `crate::theme::AGENT/SUCCESS/ERROR` const Colors. Either add new
   `ColorRole` variants (Agent / Success / ErrorMsg) or migrate when
   Snapshot goes away.
3. **`is_light()` consumers** (3): `transcript.rs:505,544`,
   `content/highlight/mod.rs:32`. Metadata flag, not a color — could
   move onto `ui::Theme` as a field, or stay on host as long as the
   atomic does.
4. **Lua bindings + tests + headless** (~14): exercise the existing
   API; will follow the API once the constants module shrinks.
5. **Bootstrap** (3 in `app/mod.rs`): accent default check at startup.
   Fine to keep.

P0 closed: 4 of 9 deletions shipped (orthogonal); 5 structural
deletions deferred to P1.0 sub-phase (paired with replacements).

**Note for next session:** puml + SVG are in sync. If the puml is
edited, regenerate via `plantuml -tsvg
refactor/tui-ui-architecture-target.puml`. Run `refactor/check.sh`
before declaring anything done.

## What's next

In order:

1. **Replace `Snapshot` with `&ui::Theme`** in the rendering pipeline.
   `crate::theme::snapshot()` is called per render; the resulting
   `Snapshot` struct is passed to `project_display_line`, content
   formatters, etc. Switch each `theme: &Snapshot` to `theme:
   &ui::Theme` and replace `theme.accent` with
   `theme.get("SmeltAccent").fg.unwrap_or_default()`. Removes the
   `Snapshot` type and the `ColorRole::Accent`/etc. resolution.
2. **Add renderer-color groups** for AGENT / SUCCESS / ERROR consumed
   by `transcript_present/*`. Either new `ColorRole` variants or
   direct `ui::Theme` lookups.
3. **Delete `crate::theme::*` constants module** when call sites drop
   to zero. Remaining `theme.rs` becomes a `default_smelt_theme()`
   builder + the `PRESETS` list + light/dark detection
   (`detect_background`, `is_light`).
4. Other P1.0 pairings (`BufferView`, `PanelWidget`/`Component`,
   `Placement`) per their target sub-phases (P1.a..P1.d).

Recently shipped: theme registry + plumbing + host bridge + 16
call site migrations. P0 orthogonal deletions
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
