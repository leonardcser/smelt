# Continue the refactor

You have **no memory** of prior iterations. The repo is your state. The rules in
`refactor/README.md` are binding — read them once, then act.

## Orient

1. `git log --oneline -20` — see what just landed.
2. `refactor/check.sh` — must be green; fix red before advancing.
3. `refactor/REFACTOR.md` — find the next un-landed sub-phase.
4. The active `refactor/P<n>.md` — latest decisions / deferrals / open questions.

## Land one sub-phase

- Plan briefly (2-3 sentences), then implement.
- Verify before finishing:
  `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo nextest run --workspace && refactor/check.sh`.
- If a non-obvious decision landed, append one bullet to `P<n>.md` "Decisions
  made": `**<title>** — <one line>.`
- Mark the sub-phase landed in `REFACTOR.md`.
- Update `INVENTORY.md` Status for touched files.
- Wait for the user's green flag before committing.
