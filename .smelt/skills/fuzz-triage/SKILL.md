---
name: fuzz-triage
description: Use when the user asks to start, continue, run, or triage Smelt fuzzing. Drives the full fuzz suite continuously: status, minimization, coverage-guided fork allocation, background runs, crash triage, fixes, regression seeds, commits, and relaunch.
---

# Smelt fuzz triage loop

Use this skill for requests like "start fuzzing", "run the fuzzers", "continue fuzzing", "triage fuzzing", or any instruction to drive the Smelt fuzz suite unattended.

The operating model is: launch fuzzers in the background, stop responding, wait for a background-process exit notification, fix exactly one crash, commit it, relaunch the crashed target, and repeat until the user gives different instructions.

Do not ask the user what to run. Do not poll, sleep, or schedule wakeups. A fuzzer exit notification is the wake signal.

## Source of truth

Do not rely on memorized target lists. At the start of each fuzzing session, read `fuzz/README.md` and use it as the source of truth for:

- the current fuzz targets,
- which targets use structured JSON scenarios versus raw bytes,
- the supported `cargo xtask fuzz ...` commands,
- regression-seed conventions.

If a detail belongs in durable project documentation, prefer updating `fuzz/README.md`; keep this skill focused on the operating loop and decision process.

## Startup checklist

Run these before launching fuzzers.

### 1. Check CPU budget

```sh
CORES=$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc --all)
echo "cores=$CORES"
```

Use the online CPU count as the total fork budget unless the user explicitly supplied another budget. Reserve one core only if the machine is struggling; otherwise spend the full budget on fuzzing.

### 2. Check current fuzzing status

Before launching anything, inspect the current state:

- already-running fuzz processes,
- corpus size per target,
- existing artifacts under `fuzz/artifacts/<target>/`,
- recent logs under `/tmp/fuzz-loop/`, if present.

If any target has an untriaged artifact, triage and fix that before starting new fuzzing. Do not bury old crashes under new runs.

### 3. Decide whether corpus minimization is needed

`cargo xtask fuzz run <target> --cmin --fork N` runs corpus minimization before fuzzing. Use `--cmin` selectively when the status check indicates it is useful:

- a target has a large or stale corpus,
- corpus preflight crashes before fork mode starts,
- coverage has plateaued while corpus size keeps growing,
- many seeds were just added or imported,
- duplicate corpus entries appear to be wasting workers.

Do not run `--cmin` for every target by default; targeted minimization is usually better.

### 4. Check which targets are increasing coverage

Use `cargo xtask fuzz coverage-snapshot` when coverage data is needed. Compare the new file in `fuzz/coverage-history/` with the previous snapshot.

Targets whose line/function/region coverage increased are worth more exploration. Targets with no corpus or no coverage movement should still get occasional runs, but they do not deserve the extra forks until they start producing signal.

## Fork allocation

Use the CPU count as the total fork budget unless the user provided a different number.

Allocate by judgment, not by a fixed table:

- give every active target at least one fork when possible,
- put more forks on expensive / high-surface-area / large-corpus targets,
- put extra forks on targets whose coverage is still increasing,
- reduce forks for targets that are tiny, saturated, or failing immediately in preflight,
- if there are fewer cores than targets, run the most valuable subset now and rotate the rest after the next crash/fix cycle.

The split should be explicit in your launch plan, but it should be derived from the current README, corpus sizes, artifact state, and coverage snapshots rather than baked into the skill.

## Launch fuzzers in background

Create a stable log directory and launch each selected target with its allocated fork count. Redirect stdout and stderr to `/tmp/fuzz-loop/fuzz-<target>.log`.

Use Smelt's background process mode for each command: call the `bash` tool with `background: true`. Do not shell-background with `&`.

When launching multiple targets, use parallel tool calls so they start together. Each command should have this shape:

```sh
cargo xtask fuzz run <target> --fork <N> > /tmp/fuzz-loop/fuzz-<target>.log 2>&1
```

Add `--cmin` only for targets selected by the minimization check.

After launching, stop. Do not read logs for progress. Do not schedule a wakeup. The background-process completion notification is the next action trigger.

## On background-process exit

A fuzzer should exit only on crash, failed corpus preflight, explicit cancellation, or environment/tooling failure.

Handle one failing target per cycle:

1. Read `/tmp/fuzz-loop/fuzz-<target>.log`.
2. Identify the artifact path. New artifacts are under `fuzz/artifacts/<target>/`.
3. If the exit was tooling/setup failure, fix the setup problem and relaunch the same target. Do not create a regression seed for non-product failures.
4. If it is a product crash, triage and minimize.

## Triage and minimization

Use the current README/xtask behavior to choose the right path.

For structured JSON scenario targets, use the xtask triage wrapper:

```sh
cargo xtask fuzz triage <target> fuzz/artifacts/<target>/crash-<hex>
```

This performs crash-to-scenario conversion, shrinking, and prints a minimal scenario. Replay the shrunk scenario directly while debugging. Use visual replay only for targets that support it.

For raw-byte targets, minimize bytes directly with `cargo fuzz tmin`, then replay the minimized artifact once to confirm it still reproduces.

Use the minimized scenario or artifact as the regression seed once the bug is fixed.

## Fixing rules

Diagnose first, fix once.

- Form a concrete root-cause hypothesis before editing.
- Do not suppress the panic, widen invariants, add broad tolerance, or paper over the symptom.
- For UTF-8 boundary issues, follow `AGENTS.md`: use `smelt_buffer::text` / `smelt_edit::text` helpers instead of raw slicing or duplicated snapping logic.
- For attached text, mutate through `Buffer::text_mut()` / `AttachedTextMut` entry points so source and attachment IDs stay in lockstep.
- For cache invariance targets, preserve prompt-cache prefix stability; inspect the provider cache tests and serialization code before changing expectations.

## Regression seed convention

Every product bug fixed from fuzzing gets a committed regression seed under:

```text
fuzz/seeds/<target>/regression/
```

For structured JSON targets, commit the minimized JSON scenario and include `_about` and `_fix` fields as described in `fuzz/seeds/README.md`.

For raw-byte targets, commit the minimized artifact directly under the target's `regression/` directory. No extension is required unless the README says otherwise.

## Verification before commit

Run focused checks first while debugging, then full checks before committing:

```sh
cargo xtask fuzz replay-regression
set -o pipefail; cargo fmt && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -120
set -o pipefail; cargo nextest run --workspace 2>&1 | tail -120
```

If `cargo nextest` is unavailable, use the repository's targeted `cargo test` fallback and keep output bounded.

## Commit style

Use the repository's existing conventional style, e.g.:

```text
fix(tui): clamp visual anchor after paste
fix(buffer): preserve attachment ids on replace
fix(cache): keep prompt cache key stable
```

One focused commit is preferred. A separate regression-seed commit is acceptable only when it makes review clearer. Do not add co-author lines. Do not mention internal agent plans.

Never push, force-push, or rebase onto remote unless the user explicitly asks.

## Relaunch loop

After the fix and seed are committed:

1. Relaunch only the target that crashed, using its previous fork count and `--cmin` only if minimization is now warranted.
2. Stop again and wait for the next background-process exit notification.
3. Repeat indefinitely until the user gives different instructions.

The other fuzzers should keep running while one crash is being triaged. Do not stop healthy fuzzers unless they block the fix or the user tells you to stop them.
