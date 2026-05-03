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
     so the user can resolve it later. Do not block the loop on it.

## Granularity

**Sub-phase is the planning unit; the session is the execution unit.**
A session lands **multiple** consecutive sub-phases — three to five
is typical, one is a failure mode. Don't split one across sessions,
don't merge two into a single commit, don't swallow whole phases in
one go.

**Don't pre-split.** Attempt the sub-phase as written. Only split it
*mid-attempt*, after you've already landed the part that fits
(`C.8` → `C.8a` / `C.8b`, land `C.8a`, continue with the next
independent sub-phase). Splitting before writing any code — the
single most common micro-session anti-pattern — turns the session
into a planning exercise that produces a docs-only commit and exits.
If the listed scope looks big, that's the session's work, not a
signal to subdivide.

**Cap nesting at 3 levels.** `P2.b.4c` is the floor. A fourth level
(`P2.b.4c.5b`) means the splitter is running away — land the work
instead of subdividing further.

**Bundle adjacent sub-phases that share seams.** If the next
sub-phase touches the same files, types, or borrow shapes as the
one you just landed, do them together. The sub-phase ID is a label;
the commit boundary is yours to choose.

## Greenness

- **Each sub-phase + its docs commit must end green.** Intermediate
  commits within a sub-phase can be red.
- **Red commits inside a session are encouraged when they avoid scaffolding.**
  If a final-shape commit can only land by leaving the tree red until the
  next commit lands the matching change, do that. The alternative — migration
  shims, parallel "kept for now" implementations, "removed in next commit"
  comments — is exactly the noise this refactor exists to delete.
- **No throwaway scaffolding.** When a step rewrites a thing, the old thing
  goes in the same sub-phase.

## Stopping rule

After a sub-phase lands green (HEAD, `cargo nextest`, `refactor/check.sh`,
`P<n>.md`, `REFACTOR.md` all in sync), **the default is to continue.**
Check the next un-landed sub-phase:

1. **Independent and unblocked** — keep going. This is the common
   case; expect to hit it 2-4 times per session.
2. **Active phase closes** — exit; the next session opens `P<n+1>`.
3. **Needs a decision you don't have** — defer dependents, log the
   question in `P<n>.md`, then **look earlier in the phase or one
   phase back for independent work**. Only exit when nothing in any
   open phase is unblocked.

A session that lands one sub-phase and exits on case 3 without
checking for independent work elsewhere is the failure mode this
rule is shaped against.

Hard stops (exit immediately, regardless of remaining work):

- **Real external blocker** — missing credentials, broken environment,
  action that genuinely requires the user (e.g. interactive
  `gh auth login`). Record in `P<n>.md` "Open questions" and exit.
- **Two consecutive failed attempts** to land a sub-phase. Exit, human
  looks.

**Don't commit deferrals as standalone work.** "Recording a design
question" is one bullet in `P<n>.md` "Open questions" — bundle it
into the next real commit, never its own `docs(refactor): record <X>`
commit. If the only thing your session produced is a deferral commit,
you've exited too early; go back to step 1.

**Ambiguity is not a stop reason** — see "The plan is not final" above. A
clear better option means you pick it; a rippling unresolved decision means
you defer the dependent sub-phases and move on to independent ones.

Do **not** stop just because a single commit landed — keep going within
the sub-phase, and keep going across sub-phases when the next is unblocked.

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

**Commit docs with the code, not separately.** The commit message carries
the detail. Only split docs into their own commit when the docs change is
large enough to stand alone (e.g. FEATURES.md refresh after multiple
features land together). Never produce a `docs(refactor): record <X>`
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
| `PROMPT.md`                       | RALPH loop entry prompt — what each session does.|
| `ralph.sh`                        | RALPH loop driver. Spawns a tmux window running `claude -p` per iteration. |
| `hooks/post_doc_edit.sh`          | PostToolUse hook. Yells when a `refactor/*.md` file exceeds its line cap. |
| `hooks/stop_gate.sh`              | Stop hook. Blocks "done" if `refactor/check.sh` is red. |

## `P<n>.md` template

```md
# P<n> — <phase name>

**Status:** in-progress | done **Started:** YYYY-MM-DD **Landed:** YYYY-MM-DD

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

No "Sub-phases landed" section — that's `git log`. The phase log records
decisions and deferrals, not commit history.
