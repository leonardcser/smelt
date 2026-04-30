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

A session lands **one or more** consecutive sub-phases. The sub-phase
is still the natural unit — never split one across sessions or merge
two into one commit. When a sub-phase finishes and the next is
independent (no shared rewrites, no blocked decisions), keep going.

Stop after a sub-phase lands when:

- The next sub-phase needs a decision you don't have (defer it, log
  the question in `P<n>.md`, look for independent work elsewhere; if
  nothing's independent, exit).
- The active phase closes (`P<n>` goes `Status: done`).
- Two consecutive failed attempts to land — exit, human looks.

If a sub-phase you started can't finish cleanly, split it in
`REFACTOR.md` (`C.8` → `C.8a` / `C.8b`), land what's done, exit.

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
