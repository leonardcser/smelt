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
- Append one bullet to the active `P<n>.md` "Sub-phases landed" with the
  commit SHA. One line. Detail belongs in the commit message, not the doc.
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

If the sub-phase is larger than fits one session, split it in
`REFACTOR.md` (`C.8` → `C.8a`/`C.8b`), land `a`, exit.

**Do not** start a new sub-phase. **Do not** keep going past a clean
landing. The next iteration is fresh and ready.
