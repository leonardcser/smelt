# RALPH — continue the refactor.

You are one iteration of a self-driving loop. You have **no memory** of prior
iterations. The repo is your state. The rules in `refactor/README.md` are
binding — read them once, then act.

## Orient

Run these two commands first and read their output — git log is your
primary source of truth for what just landed; do not skip it:

1. `git log --oneline -20` — see what just landed.
2. `refactor/check.sh` — must be green; fix red before advancing.

Then read:

3. `refactor/REFACTOR.md` — find the next un-landed sub-phase.
4. The active `refactor/P<n>.md` — latest decisions / deferrals / open
   questions.

## Land sub-phases

**A session lands multiple consecutive sub-phases — three to five is
typical, one is a failure mode.** The sub-phase is the planning unit;
the session is the execution unit. Default is to keep going; exit is
the exception.

**Don't pre-split.** Attempt the sub-phase as written. If it turns
out too large, split *after* you've landed the part that fits
(`C.8` → `C.8a` / `C.8b`, land `C.8a`, continue with the next
independent sub-phase). Splitting before writing code is the planning
anti-pattern this loop most often falls into — it produces sessions
whose only output is a docs commit recording the split.

**Bundle when seams overlap.** If the next sub-phase touches the same
files, types, or borrow shapes you just edited, do both in this
session. The sub-phase ID is a label; the commit boundary is yours.

**No 4-level IDs.** `P2.b.4c` is the floor. A fourth nesting level
means you're over-splitting — land the work instead.

**Don't commit deferrals as standalone work.** "Recording a question"
is one bullet in `P<n>.md` bundled into the next real commit — never
its own `docs(refactor): record <X> question` commit. If your session
would otherwise produce only a deferral commit, you've exited too
early; go find independent work in this phase or an earlier one.

Exit only when:

- The active phase closes (`P<n>` goes `Status: done`).
- Every remaining sub-phase across all open phases is blocked on an
  unresolved decision (check earlier phases before concluding this).
- Two consecutive failed attempts to land — human looks.

For each sub-phase:

- Plan briefly (2-3 sentences), then implement.
- Intermediate commits can be red. The sub-phase + its docs commit
  must end green. Prefer red-mid-sub-phase over shims.
- Verify after each: `cargo fmt && cargo clippy --workspace
  --all-targets -- -D warnings && cargo nextest run --workspace &&
  refactor/check.sh`.
- Append one bullet to `P<n>.md` "Sub-phases landed":
  `**<id>** (\`<sha>\`) — <one-line summary>.` **One line.** Body
  lives in `git log`; this file is an index.
- If a non-obvious decision landed, one bullet in "Decisions made":
  `**<title>** (\`<sha>\`) — <one line>.` Same rule.
- Mark the sub-phase landed in `REFACTOR.md`.
- Update `INVENTORY.md` Status for touched files. Notes column is
  forward-looking only — pending/blocking, not history.
- Commit code as `feat(tui): …` / `refactor(ui): …`; docs sync as a
  separate `docs(refactor): …` immediately after.

## Loop sentinels

Two files signal the loop driver to exit cleanly. Write at most one,
only when its precondition is genuinely true.

### `refactor/.ralph-done` — plan complete

Every phase in `REFACTOR.md` is `Status: done`, no `P<n>.md` has open
sub-phases, no "Open questions" section has unresolved entries.
Contents: one-line summary (e.g. `P7 landed; full refactor green`).

### `refactor/.ralph-needs-input` — your turn

Every un-landed sub-phase across all phases is deferred on an open
question recorded in some `P<n>.md`. No independent work left for a
fresh agent. Contents: one-line summary pointing at the deciding
questions (e.g. `P3.b naming + P5.b dispatcher wiring blocked on user`).

**Do not write either file** in any other situation. Partial completion,
deferrals with independent work, "I think we're close", or "I tried and
failed" all stay implicit — let the no-commit branch surface as `stuck`.
