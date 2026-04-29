# Status

Where we are right now. Updated at session end.

For the entry point and meta-rules, read `README.md` first.

## Where we are

**Phase:** P0 landed (orthogonal deletions). Five structural deletions
deferred to P1.0, paired with their replacements.

**Tree:** green. `cargo nextest run --workspace` — 901 passed.
`cargo nextest run --test scenarios` — 6 baseline scenarios green
(5 regression gates + smoke).

**Last update:** 2026-04-29. P0 landed across three commits: yank /
mouse-with-lua removal earlier, `selection_style` fields (`4a2e368`),
`BufferList` (`60db49f`). Strategy shifted mid-phase: original plan
ended P0 red, but the four remaining structural deletions
(`BufferView`, theme constants, `PanelWidget` multiplexing, `Component`
trait, `Placement` enum) couldn't land without their P1 replacements
existing. Rather than burn the green-tree baseline, those moved to a
new P1.0 sub-phase that pairs each deletion with its replacement in
the same commit. See `P0.md` "Decisions made while coding".

**Note for next session:** puml + SVG are in sync. If the puml is
edited, regenerate via `plantuml -tsvg
refactor/tui-ui-architecture-target.puml`. Run `refactor/check.sh`
before declaring anything done.

## What's next

In order:

1. **Start P1.0** — pair each deferred structural deletion with its
   replacement. First commit is the smallest: `ui::Theme` registry +
   `crates/tui/src/theme.rs` constants module deletion in one go
   (tracked task `20260426-083607`). Then P1.a..P1.d absorb the
   remaining four deletions as part of their primitive landings.
2. **P1.a (`Buffer` rewrite)**, **P1.b (`LayoutTree`)**, **P1.c
   (`Overlay`)**, **P1.d (`Window` as the only interactive unit)**
   per `REFACTOR.md` § P1.

Recently shipped: P0 orthogonal deletions
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
