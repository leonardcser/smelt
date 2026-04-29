# Status

Where we are right now. Updated at session end.

For the entry point and meta-rules, read `README.md` first.

## Where we are

**Phase:** pre-P0 (test baseline harness landing; demolition not
started).

**Tree:** green. All features working. Two integration scenarios
landed (`plain_turn`, `thinking_then_text`) — both green and
deterministic. ~3 to 8 more scenarios still want to land before P0.

**Last update:** 2026-04-29. Testing strategy doc + harness
scaffolding committed (`dd86689`). First two scenarios committed
(`22e3d74`): wiremock'd Anthropic SSE → headless smelt → `insta`
snapshot of the JSONL event stream. Determinism handled via
redactions on `elapsed_ms` / `avg_tps` / `tokens_per_sec`.

**Note for next session:** puml + SVG are in sync. If the puml is
edited, regenerate via `plantuml -tsvg
refactor/tui-ui-architecture-target.puml`. Run `refactor/check.sh`
before declaring anything done. Re-run snapshot tests with
`cargo nextest run --test scenarios` and review diffs by inspecting
`tests/snapshots/*.snap.new` (no `cargo-insta` binary installed in
this worktree — bless by `mv` or install it).

## What's next

In order:

1. **Finish the baseline scenario set.** `plain_turn` and
   `thinking_then_text` are in. Still want roughly: incomplete stream,
   provider error path, plugin tool happy-path (Lua tool + hook,
   tests the dispatch round-trip), permission deny, mode change.
   Goal is 5–10 scenarios that lock the behaviours most likely to
   silently shift during demolition. Stop when coverage feels
   adequate, not when a fixed count is hit.
2. **Start P0** — clear the deck. Delete BufferView, Component,
   PanelWidget, the 6-variant Placement, theme constants, scattered
   selection_style, MouseAction::Yank, etc. End state: red tree, clean
   bones. Write `refactor/P0.md`.

Recently shipped: `TRACE.md` (vertical-slice walk-through, also
serves as the concrete `init.lua` + `bash.lua` API example). Five
small design holes the trace surfaced got fixed in the canonical docs
in the same commit. `TESTING.md` (three-layer testing strategy:
marker DSL for model state / headless+wiremock for engine integration
/ grid render + Pilot for rendering) added as the canonical testing
reference. Test harness + first two scenarios (`plain_turn`,
`thinking_then_text`) landed; goldens are deterministic across runs.

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

- _(no phases landed yet)_

## How to keep this file current

- At session end: update "Where we are", "What's next", "Open
  questions" — even one line is enough.
- When a phase boundary lands: rotate the line into "Recently landed",
  reset "What's next."
- When a decision is made: log it in the active `P<n>.md`; if it
  changes scope, also update `REFACTOR.md` in the same commit.

(For meta-rules and the cold-start checklist, see `README.md`.)
