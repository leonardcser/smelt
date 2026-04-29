# Status

Where we are right now. Updated at session end.

For the entry point and meta-rules, read `README.md` first.

## Where we are

**Phase:** P1.0 in progress. Theme registry foundation landed; call
site migration pending.

**Tree:** green. `cargo nextest run --workspace` — 908 passed (901
from P0 boundary + 7 new theme registry tests). `cargo nextest run
--test scenarios` — 6 baseline scenarios green.

**Last update:** 2026-04-29. P1.0 opened with three commits laying the
theme infrastructure: `ui::Theme` registry type (`decb0ab`), plumbed
through `DrawContext` and owned by `Ui` (`177ac4c`), populated from
host `crate::theme::*` constants at startup (`bb9cc63`). The registry
runs in parallel with the existing flat module — both are populated
with the same values, so widgets can opt in to `ctx.theme.get(name)`
without breaking anything that still reads `crate::theme::accent()`
etc. Once all 50 call sites migrate, `crates/tui/src/theme.rs` can
shrink to a default-theme builder + preset list and the constants
module is gone.

P0 closed: 4 of 9 deletions shipped (orthogonal); 5 structural
deletions deferred to P1.0 sub-phase (paired with replacements).

**Note for next session:** puml + SVG are in sync. If the puml is
edited, regenerate via `plantuml -tsvg
refactor/tui-ui-architecture-target.puml`. Run `refactor/check.sh`
before declaring anything done.

## What's next

In order:

1. **Migrate `crate::theme::*` call sites to `ctx.theme.get(...)`**
   one file at a time. Easy ones first: `app/render_loop.rs`
   `selection_bg()` → `ctx.theme.get("Visual").bg`; `app/status_bar.rs`
   accent + agent + muted; `format.rs` snapshot consumers. Hot paths
   that build snapshots take `&Theme` as a parameter instead of
   reading global atomics.
2. **Hook the runtime mutators** (`set_accent`, `set_light`,
   `/theme preset`) to call `populate_ui_theme(ui.theme_mut())` after
   the atomic update so the registry stays in sync.
3. **Delete the `crate::theme::*` constants module** when call sites
   drop to zero. The remaining `theme.rs` becomes a `default_smelt_theme()` builder + the `PRESETS` list + light/dark
   detection (`detect_background`, `is_light`).
4. Other P1.0 pairings (`BufferView`, `PanelWidget`/`Component`,
   `Placement`) per their target sub-phases (P1.a..P1.d).

Recently shipped: theme registry foundation (`ui::Theme` +
plumbing + host bridge). P0 orthogonal deletions
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
