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

3. `refactor/REFACTOR.md` — find the next un-landed sub-phase under the
   active phase. That sub-phase is your scope for this session.
4. The active `refactor/P<n>.md` — see the latest decisions, deferrals,
   open questions.

## Land one sub-phase

- Plan briefly (2-3 sentences), then implement.
- Intermediate commits can be red. The session must end green.
  Encourage red-mid-session over scaffolding / shims / "kept for now".
- Verify at session end: `cargo fmt && cargo clippy --workspace
  --all-targets -- -D warnings && cargo nextest run --workspace &&
  refactor/check.sh`.
- Append one bullet to the active `P<n>.md` "Sub-phases landed":
  `**<id>** (\`<sha>\`) — <one-line summary>.`
  **One line.** No paragraphs, no nested bullets, no rephrasing the
  commit body. The body lives in `git log`; this file is an index.
  If you can't fit the summary on one line, you're duplicating the
  commit message — trim.
- If the sub-phase produced a non-obvious decision worth re-litigating
  later, add one bullet to "Decisions made":
  `**<title>** (\`<sha>\`) — <one line>.` Same one-line rule.
  Reasoning lives in the commit message, not here.
- Mark the sub-phase landed in `REFACTOR.md`.
- Update `INVENTORY.md` Status columns for files you touched.
- Commit with conventional commits (`feat(tui): …`, `refactor(ui): …`).

## Stop

Per `refactor/README.md` § Stopping rule:

1. Sub-phase landed (tree green, docs synced, commit on HEAD) — exit.
2. Real external blocker (missing credentials, environment broken,
   user action genuinely required) — record in `P<n>.md` "Open
   questions", exit.
3. Two consecutive failed attempts to land — exit, human looks.

**Never stop to ask the user.** If a decision is clear, pick the better
option and log it. If a big decision ripples and no winner is clear,
defer the dependent sub-phases, record the question in `P<n>.md`, and
move on to independent work.

## Loop sentinels

Two files signal the loop driver to exit cleanly. Write at most one,
only when its precondition is genuinely true.

### `refactor/.ralph-done` — plan complete

Write this when every phase in `REFACTOR.md` is `Status: done`, no
`P<n>.md` has open sub-phases, and no `P<n>.md` "Open questions"
section has unresolved entries. Contents: a one-line summary
(e.g. `P7 landed; full refactor green`).

### `refactor/.ralph-needs-input` — your turn

Write this when **every un-landed sub-phase across all phases** is
deferred on an open question recorded in some `P<n>.md`. That is:
there is no independent work left for a fresh agent to attempt — every
remaining path needs a user decision before it can move. Contents: a
one-line summary pointing at the deciding questions (e.g.
`P3.b naming + P5.b dispatcher wiring blocked on user`).

**Do not write either file** in any other situation. Partial
completion, deferrals with independent work still available, "I think
we're close", or "I tried and failed" all stay implicit — leave no
sentinel and let the no-commit branch surface as `stuck`. Writing the
wrong sentinel ends the loop prematurely.

If the sub-phase is larger than fits one session, split it in
`REFACTOR.md` (`C.8` → `C.8a`/`C.8b`), land `a`, exit.

**Do not** start a new sub-phase. **Do not** keep going past a clean
landing. The next iteration is fresh and ready.
