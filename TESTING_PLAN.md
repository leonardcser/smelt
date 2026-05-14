# Testing Plan

Working document. Updated as work lands. Principles are in [`TESTING.md`](./TESTING.md).

## Baseline (2026-05-14)

Workspace line coverage: **52.7%**. 1 055 tests, 139 snapshots.

| Crate    | Line% |    LoC | Shape                                                |
|----------|------:|-------:|------------------------------------------------------|
| `style`  | 91.4% |    390 | pure                                                 |
| `edit`   | 86.8% | 10 440 | pure (motions, vim, text objects)                    |
| `term`   | 86.8% |  3 294 | pure layout + grid                                   |
| `core`   | 50.5% | 21 951 | mixed; permissions strong, I/O glue weak             |
| `engine` | 40.1% |  8 225 | pure parts strong; provider HTTP/SSE ~0%             |
| `tui`    | 36.5% | 26 958 | imperative shell; `app/*` dispatchers ~0%            |
| `protocol` | 40.0% |  922 | data types                                           |

## Status legend

`☐` not started · `◐` in progress · `☑` done · `✗` skipped · `⏸` blocked

## Per-crate substeps

Every crate goes through the same seven substeps. Order matters: think, then implement.

1. **Read** — walk the crate's source; map ring vs core; spot tangled decisions.
2. **Audit existing tests** — apply the checklist below; produce four lists (delete / rename / strengthen / consolidate).
3. **Propose plan** — write down what to refactor and what to add. **Sign-off before touching code.**
4. **Apply audit** — delete + rename + strengthen + consolidate.
5. **Refactor ring/core** — extract pure decisions; ring becomes thin "compute then apply."
6. **Fill behavioural coverage** — table-driven where finite; one test = one user-visible guarantee.
7. **Verify** — `cargo llvm-cov nextest -p <crate> --summary-only`; sign-off; move to next crate.

### Audit checklist (used in step 2)

- **Litmus.** Delete the test — can I describe what behaviour stopped being guaranteed? If no → delete.
- **Name.** Describes a user-visible behaviour, not an internal function?
- **Assertion strength.** Asserts on outputs, not internals? No `is_ok()`/`!is_empty()`/"doesn't panic"-only.
- **Setup weight.** Heavy scaffolding → flag the seam for the refactor step.
- **Overlap.** N tests on the same branch → consolidate into one table-driven case.
- **Determinism.** No `sleep`, wall-clock, random, real I/O.
- **Snapshot quality.** `.snap` captures behaviour, not noise. If it churns on every refactor → it's pinning implementation.

## Order of crates

User-facing first, but foundations before the shell to avoid re-doing work:

1. **`style`** — warmup. Tiny, pure, already at 91%. Validates the workflow.
2. **`term`** — UI primitives. 87%. Quick audit + light fill.
3. **`buffer`** — text storage. Audit + fill `undo`, `kill_ring`.
4. **`edit`** — vim/motions. 87%. Audit + direct `motions` tests.
5. **`tui`** — the big one. Full A/B/C: dispatchers, `TestApp` harness, fill.
6. **`core`** — content / permissions / session. Audit + extract + fill.
7. **`engine`** — providers. Audit + provider extraction + fill.

Skipped: `protocol` (derives only), `perf`, `xtask`, `lua-doc-derive` (tooling).

## Crate boards

Each box represents one substep. Fill known work items under "Known work" as they're surfaced; check off as completed.

---

### `style` — ☑  ·  91.4% → **97.6%** (target ≥90%)

1 · ☑ Read   2 · ☑ Audit   3 · ☑ Plan   4 · ☑ Apply   5 · n/a   6 · ☑ Fill   7 · ☑ Verify

**Outcome:** 9 → 19 tests. Audit found no deletes/renames/consolidates needed; strengthened the cycle test with a long-chain depth-cap case. Filled interner invariants (stability, distinctness, round-trip), anon-style id equality + Theme fallthrough, `contains`, and a `Style` builder round-trip. Pure crate; no ring/core refactor.

---

### `term` — ☐  ·  86.8% → ≥90%

1 · ☐ Read   2 · ☐ Audit   3 · ☐ Plan   4 · ☐ Apply   5 · n/a   6 · ☐ Fill   7 · ☐ Verify

**Known work:**
- Add proptest on `term::layout` (sum of widths == container).
- Storybook quality check.

---

### `buffer` — ☐  ·  (mixed) → ≥90%

1 · ☐ Read   2 · ☐ Audit   3 · ☐ Plan   4 · ☐ Apply   5 · n/a   6 · ☐ Fill   7 · ☐ Verify

**Known work:**
- `undo.rs` (0 tests) — pure data struct, ~10 cases.
- `kill_ring.rs` — verify coverage; fill if needed.
- Proptest on `text::safe_*` (never panics).

---

### `edit` — ☐  ·  86.8% → ≥90%

1 · ☐ Read   2 · ☐ Audit   3 · ☐ Plan   4 · ☐ Apply   5 · n/a   6 · ☐ Fill   7 · ☐ Verify

**Known work:**
- `motions.rs` (0 direct tests; tested only via `vim`) — direct table-driven tests.
- `text_objects.rs` — verify direct coverage.
- Proptest: motions stay within line; idempotence where defined.

---

### `tui` — ☐  ·  36.5% → ≥65%

1 · ☐ Read   2 · ☐ Audit   3 · ☐ Plan   4 · ☐ Apply   5 · ☐ Refactor   6 · ☐ Fill   7 · ☐ Verify

The largest job. Substep 5 is the bulk of the work and breaks down into the dispatcher extractions below.

**Dispatcher `route()` extractions (substep 5):**

| # | Module                                | LoC | Cov | Status |
|---|---------------------------------------|----:|----:|:------:|
| 1 | `app/events.rs` (template)            | 605 |  0% |  ☐    |
| 2 | `app/cmdline.rs`                      | 322 |  0% |  ☐    |
| 3 | `app/content_keys.rs`                 | 182 |  0% |  ☐    |
| 4 | `app/mouse.rs`                        | 200 |  0% |  ☐    |
| 5 | `picker.rs`                           | 218 |  0% |  ☐    |
| 6 | `commands.rs`                         | 208 |  0% |  ☐    |
| 7 | `app/engine_events.rs`                | 286 |  0% |  ☐    |
| 8 | `app/agent.rs`                        | 615 |  0% |  ☐    |

**`TestApp` harness (substep 5):**

| # | Item                                  | Status |
|---|---------------------------------------|:------:|
| 1 | Skeleton: `feed(events)`, `state()`, `actions()` |  ☐ |
| 2 | First suite: overlays, cmdline, Ctrl-C semantics |  ☐ |
| 3 | Picker open → filter → select         |  ☐    |
| 4 | Vim mode transitions end-to-end       |  ☐    |

**Lower-priority within tui (do after dispatchers):**
- `input/buffer.rs` (34%) — make `PromptCtx<'_>` constructible from in-memory state.
- `metrics.rs` (0%) — extract pure accumulators.
- `content/transcript_parsers/tools.rs` (26%) — fill parser branches.

---

### `core` — ☐  ·  50.5% → ≥70%

1 · ☐ Read   2 · ☐ Audit   3 · ☐ Plan   4 · ☐ Apply   5 · ☐ Refactor   6 · ☐ Fill   7 · ☐ Verify

**Known work:**
- `content/builder.rs` (630 LoC, 0 tests) — pure layout primitive. Fill.
- `working.rs` (357 LoC, 0%) — extract pure file-change index from ring shell.
- `mcp/mod.rs` (267 LoC, 0%) — ring; trust the wire, extract any pure conversions.
- Verify `permissions/*` audit (147 tests, but quality unknown).

---

### `engine` — ☐  ·  40.1% → ≥75%

1 · ☐ Read   2 · ☐ Audit   3 · ☐ Plan   4 · ☐ Apply   5 · ☐ Refactor   6 · ☐ Fill   7 · ☐ Verify

**Provider extraction (substep 5):**
Replicate the shape of `provider/extract.rs` (95% cov) on each provider — pull `build_body`, `parse_response`, `parse_sse_event`, `auth_headers` into pure free functions.

| # | Module                              | LoC   | Cov  | Status |
|---|-------------------------------------|------:|-----:|:------:|
| 1 | `provider/anthropic.rs`             |   285 |  ~0% |  ☐    |
| 2 | `provider/openai.rs`                |   342 |  ~0% |  ☐    |
| 3 | `provider/codex.rs`                 |   653 |  ~0% |  ☐    |
| 4 | `provider/chat_completions.rs`      |   210 |  ~0% |  ☐    |
| 5 | `provider/copilot.rs`               |   591 | small|  ☐    |
| 6 | `provider/mod.rs`                   | 1 047 |   0% |  ☐    |
| 7 | `provider/sse.rs` (45 LoC, 0%)      |    45 |   0% |  ☐    |

**Fuzz target (substep 6):** `engine::sse` — adversarial bytes, no panic, no infinite loop.

---

## Workspace-level items (after crate work)

| # | Item                                | Status | Notes |
|---|-------------------------------------|:------:|-------|
| 1 | CI: `cargo llvm-cov nextest --workspace --fail-under-lines 52` | ☐ | Ratchet floor after each crate completes. |
| 2 | Storybook snapshot diffs in CI      |  ☐    | Already works locally. |
| 3 | Fuzz: transcript / markdown parsers |  ☐    | After `core` is done. |
| 4 | Property tests round (cross-crate)  |  ☐    | After foundations are clean. |

## Now

**Active crate:** `term` — substep 1 (Read).

Next action: read `crates/term/src/*`, map ring vs core (mostly pure layout + grid; some surface/flush will touch crossterm), and produce the audit lists for substep 2.
