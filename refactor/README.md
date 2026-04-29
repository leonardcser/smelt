# refactor/ — start here

Entry point for the smelt rebuild. Short by design.

## What this is

A multi-phase rewrite of smelt's Rust + Lua surface to the target shape in
`tui-ui-architecture-target.puml`. Greenfield: no users, no migration story,
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
     so the user can resolve it later. Do not block the loop on it.

## Granularity

**One session = one sub-phase.** Sub-phases are the natural unit
(`C.7.3`, `C.8`, `C.9`, …). Don't fragment into per-commit sessions; don't
swallow whole phases in one. If a sub-phase turns out larger than fits one
session, split it in `REFACTOR.md` (e.g. `C.8` → `C.8a` / `C.8b`), land
`C.8a`, exit. The split is part of the work.

## Greenness

- **Sessions and phases must end green.** Intermediate commits don't.
- **Red commits inside a session are encouraged when they avoid scaffolding.**
  If a final-shape commit can only land by leaving the tree red until the
  next commit lands the matching change, do that. The alternative — migration
  shims, parallel "kept for now" implementations, "removed in next commit"
  comments — is exactly the noise this refactor exists to delete.
- **No throwaway scaffolding.** When a step rewrites a thing, the old thing
  goes in the same session.

## Stopping rule

Stop when one of:

1. The active sub-phase landed: tree green at HEAD, `cargo nextest` green,
   `refactor/check.sh` green, `P<n>.md` updated, the sub-phase entry in
   `REFACTOR.md` marked landed.
2. **Real external blocker** — missing credentials, broken environment, an
   action that genuinely requires the user to do something at their machine
   (e.g. interactive `gh auth login`). Record what's needed in `P<n>.md`
   "Open questions" and exit.
3. Two consecutive failed attempts to land the active sub-phase. Exit,
   human looks.

**Ambiguity is not a stop reason** — see "The plan is not final" above. A
clear better option means you pick it; a rippling unresolved decision means
you defer the dependent sub-phases and move on to independent ones.

Do **not** stop just because a single commit landed — keep going within the
sub-phase. Do **not** start a new sub-phase in the same session.

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

## Verify when it makes sense

- **At phase boundaries:**
  `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo nextest run --workspace`.
- **Whenever a `refactor/*` doc changes:** `refactor/check.sh`. All green;
  warnings fine; red `✗` means a doc lies.
- **For UI changes:** drive the binary in a tmux side-pane (see
  `ARCHITECTURE.md` § Testing TUI changes).
- **For puml edits:** regenerate the SVG
  (`plantuml -tsvg refactor/tui-ui-architecture-target.puml`).

## The documents

| File                              | What it is                                       |
| --------------------------------- | ------------------------------------------------ |
| `README.md` (this)                | Meta rules + index.                              |
| `REFACTOR.md`                     | Sequencing — phases P0..P7 and their content.    |
| `ARCHITECTURE.md`                 | Target intent — decisions, rationale.            |
| `tui-ui-architecture-target.puml` | Canonical structure diagram.                     |
| `INVENTORY.md`                    | Per-file ledger: every source file → its fate.   |
| `FEATURES.md`                     | User-facing parity checklist.                    |
| `P<n>.md`                         | Per-phase log: what landed + decisions made.     |
| `DECISIONS.md`                    | Pre-P0 architectural decisions log (frozen).     |
| `TRACE.md`                        | One vertical slice end-to-end through target.    |
| `TESTING.md`                      | Three-layer testing strategy.                    |
| `check.sh`                        | Drift-detection invariants.                      |
| `PROMPT.md`                       | RALPH loop entry prompt — what each session does.|
| `ralph.sh`                        | RALPH loop driver. Spawns a tmux window running `claude -p` per iteration. |
| `hooks/post_doc_edit.sh`          | PostToolUse hook. Yells when a `refactor/*.md` file exceeds its line cap. |
| `hooks/stop_gate.sh`              | Stop hook. Blocks "done" if `refactor/check.sh` is red. |

## `P<n>.md` template

```md
# P<n> — <phase name>

**Status:** in-progress | done **Started:** YYYY-MM-DD **Landed:** YYYY-MM-DD

## Sub-phases landed

One bullet per sub-phase: `**<id>** (\`<sha>\`) — <one-line summary>.`
The commit message carries the detail; this file is an index.

## Decisions made

One bullet per non-obvious choice: `**<title>** (\`<sha>\`) — <one line>.`
If you'd re-litigate this in `P<n+2>`, write it down here. The body of
the reasoning lives in the commit message, not here.

## Deferrals

What slipped to a later phase, with the new home.

## Open questions

Anything blocked. Move resolved items into the relevant decision bullet
when they unblock.

## Next phase entry conditions

What must be true before `P<n+1>` starts.
```

Body of every entry is one line. Detail lives in `git log` — don't duplicate.
