# refactor/ — start here

Entry point for the smelt rebuild. Short by design.

## What this is

A multi-phase rewrite of smelt's Rust + Lua surface to the target shape in
`tui-ui-architecture.puml`. Greenfield: no users, no migration story,
no backward compatibility. The only artefact that matters is the final shape.

## The plan is not final

Every `refactor/` doc is a sketch, not a contract — sequencing
(`REFACTOR.md`), intent (`ARCHITECTURE.md`), structure (the puml + SVG),
file fates (`INVENTORY.md`), parity (`FEATURES.md`), test strategy
(`TESTING.md`), the vertical slice (`TRACE.md`), historical decisions
(`DECISIONS.md`). Reality will push back. When it does:

- **Take the better path.** Don't follow the doc when a better way is plain.
  Update the affected docs in the same change. Log the decision in `P<n>.md`.
- **Never stop to ask the user.** The user may not be at the keyboard.
  Decisions resolve in one of two ways:
  1. **A clearly better option exists** — pick it, log the choice in `P<n>.md`,
     keep going. Don't deliberate.
  2. **A big decision ripples through many sub-phases and no winner is clear** —
     defer the dependent sub-phases. Move on to independent ones in the
     phase (or earlier in the next phase if nothing in the active phase is
     unblocked). Record the open question in `P<n>.md` "Open questions"
     so the user can resolve it later. Do not block on it.

## Granularity

Sub-phase is the planning unit. Don't pre-split: attempt the sub-phase as
written. Cap nesting at 3 levels (`P2.b.4c` is the floor).

## Greenness

- Each sub-phase must end green.
- No throwaway scaffolding. When a step rewrites a thing, the old thing goes
  in the same sub-phase.

## Code stays phase-agnostic

The code never references `P0` / `P3.b` / "L2" or any other refactor-stage
label — not in comments, identifiers, test names, or commit messages beyond
the `P<n>.md` log. Source reads as if the new shape was always there.

**Carve-out:** `TODO(P<n>): <action>` is allowed at the call site to mark
"blocked until phase Pn lands the prerequisite." Keep the action concrete
(`TODO: mount Anthropic SSE cassette`) — never a lament.

## Phase transitions

When the active phase closes:

1. Mark `P<n>.md` `Status: done` and fill `Landed:` with the date.
2. Create `P<n+1>.md` from the template at the bottom of this file.
3. In `AGENTS.md`, swap `@refactor/P<n>.md` for `@refactor/P<n+1>.md`.
   Closed phase files stay on disk but stop auto-loading.
4. Update `REFACTOR.md` if `P<n+1>`'s shape diverged from what was sketched.

## Doc sync rule

When a decision is made or design shifts, update **every** affected doc in
the **same change**. Drifted docs are bugs. `check.sh` catches the
mechanical cases; the human-shaped cases (intent vs structure consistency)
need attention.

A phase isn't landed until `P<n>.md` is written and companion files reflect
what happened.

**Only commit when the user gives you the green flag.** Bundle docs with the
code they describe in the same commit. Only split docs into their own commit
when the docs change is large enough to stand alone (e.g. FEATURES.md refresh
after multiple features land together). Never produce a `docs(refactor): record <X>`
commit whose entire diff is one bullet in `P<n>.md`.

## Verify when it makes sense

- **At phase boundaries:**
  `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo nextest run --workspace`.
- **Whenever a `refactor/*` doc changes:** `refactor/check.sh`. All green;
  warnings fine; red `✗` means a doc lies.
- **For UI changes:** drive the binary in a tmux side-pane (see
  `ARCHITECTURE.md` § Testing TUI changes).
- **For puml edits:** regenerate the SVG
  (`plantuml -tsvg refactor/tui-ui-architecture.puml`).

## The documents

| File                              | What it is                                       |
| --------------------------------- | ------------------------------------------------ |
| `README.md` (this)                | Meta rules + index.                              |
| `REFACTOR.md`                     | Sequencing — phases P0..P8 and their content.    |
| `ARCHITECTURE.md`                 | Target intent — decisions, rationale.            |
| `tui-ui-architecture.puml` | Canonical structure diagram.                     |
| `INVENTORY.md`                    | Per-file ledger: every source file → its fate.   |
| `FEATURES.md`                     | User-facing parity checklist.                    |
| `P<n>.md`                         | Per-phase log: what landed + decisions made.     |
| `DECISIONS.md`                    | Pre-P0 architectural decisions log (frozen).     |
| `TRACE.md`                        | One vertical slice end-to-end through target.    |
| `TESTING.md`                      | Three-layer testing strategy.                    |
| `check.sh`                        | Drift-detection invariants.                      |
| `PROMPT.md`                       | Agent session entry prompt — orient, land one sub-phase. |
| `PROMPT_RALPH.md`                 | Legacy RALPH self-driving loop prompt (multi-sub-phase, auto-commit). |
| `ralph.sh`                        | Legacy loop driver. Spawns a tmux window running `claude -p` per iteration. |
| `hooks/post_doc_edit.sh`          | PostToolUse hook. Yells when a `refactor/*.md` file exceeds its line cap. |
| `hooks/stop_gate.sh`              | Stop hook. Blocks "done" if `refactor/check.sh` is red. |

## `P<n>.md` template

```md
# P<n> — <phase name>

**Status:** in-progress | done **Started:** YYYY-MM-DD **Landed:** YYYY-MM-DD

## Decisions made

One bullet per non-obvious choice: `**<title>** (\`<sha>\`) — <one line>.`
Omit the sha if the commit hasn't landed yet. If you'd re-litigate this in
`P<n+2>`, write it down here. The body of the reasoning lives in the commit
message, not here.

## Deferrals

What slipped to a later phase, with the new home.

## Open questions

Anything blocked. Move resolved items into the relevant decision bullet
when they unblock.

## Next phase entry conditions

What must be true before `P<n+1>` starts.
```

No "Sub-phases landed" section — that's `git log`. The phase log records
decisions and deferrals, not commit history.
