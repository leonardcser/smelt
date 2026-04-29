# Status

Where we are right now. Updated at session end.

For the entry point and meta-rules, read `README.md` first.

## Where we are

**Phase:** P1.0 in progress. Theme registry foundation landed; call
site migration pending.

**Tree:** green. `cargo nextest run --workspace` — 908 passed (901
from P0 boundary + 7 new theme registry tests). `cargo nextest run
--test scenarios` — 6 baseline scenarios green.

**Last update:** 2026-04-29. P1.0 theme registry landing across 10
commits (`decb0ab`..`1786716`):

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

Migration tally: 28 of 50 `crate::theme::*` call sites converted; 22
remain. The remaining sites are essential host-module surface that
the registry depends on, not migration targets per se:

- **Atomic plumbing** (5): `populate_ui_theme()` calls in
  `format.rs:102`, `to_buffer.rs:159` (test fixture),
  `app/mod.rs:841`, `app/render_loop.rs:13`; `detect_background()` in
  `app/mod.rs:840`. These keep registry and atomics coherent each
  frame.
- **Atomic mutators** (7): `accent_value`/`set_accent` at startup
  (`app/mod.rs:535,537`), in Lua bindings (`lua/api/mod.rs:189`,
  `lua/api/widgets.rs:35`), and 4 test sites in `lua/mod.rs`.
- **Light/dark metadata** (5): `is_light()` reads in
  `transcript.rs:505,544`, `content/highlight/mod.rs:32`,
  `lua/api/widgets.rs:64`, `status_bar.rs:74` slug fallback.
- **Preset list** (2): `lua/api/mod.rs:139` `preset_by_name`,
  `lua/api/widgets.rs:73` PRESETS iter — both legitimate uses of
  static config data.
- **Headless logs** (3): `app/headless.rs:122,125,145` — ANSI escape
  sequences for stderr logs; no Ui access; pure const colors.

Truly deleting `crates/tui/src/theme.rs` requires moving the atomic
state (accent/slug/light) into `ui::Theme` itself, which is a
separate sub-phase of P1.0. For now, the host module's role is
narrowed: it's the atomic state holder + light/dark detector +
preset list; the registry is the lookup surface every renderer reads
through.

P0 closed: 4 of 9 deletions shipped (orthogonal); 5 structural
deletions deferred to P1.0 sub-phase (paired with replacements).

**Note for next session:** puml + SVG are in sync. If the puml is
edited, regenerate via `plantuml -tsvg
refactor/tui-ui-architecture-target.puml`. Run `refactor/check.sh`
before declaring anything done.

## What's next

In order:

1. **Move atomic theme state onto `ui::Theme`**. `ACCENT_VALUE`,
   `SLUG_COLOR_VALUE`, `LIGHT_THEME` atomics in `crate::theme::*`
   become methods on `ui::Theme` (e.g. `set_accent(u8)` rebuilds the
   relevant Style entries; `is_light()` reads a flag). Once that
   lands, `populate_ui_theme()` no longer reads from atomics — it
   reads from Theme itself. The host module shrinks to: light/dark
   detection (one-shot at startup), the `PRESETS` const list, and
   the headless ANSI helpers. `Snapshot` is already gone.
2. **Other P1.0 pairings** (`BufferView`, `PanelWidget`/`Component`,
   `Placement`) per their target sub-phases (P1.a..P1.d).

Recently shipped: theme registry + plumbing + host bridge + 28
call site migrations + Snapshot elimination + ColorRole expansion.
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
