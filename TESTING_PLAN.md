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

### Writing new tests (used in step 6)

**Spec first, then code.** Write what the code *should* do, then the test, then run it. Don't read the implementation to figure out the expected output and mirror it back. When a test fails, treat it as a real question: is my expectation wrong, or is the code wrong?

### Bugs surfaced during testing

Writing tests against expected behaviour will turn up surprises. Two kinds:

- **Clearly a bug** (incorrect rendering, panic, broken invariant, two code paths giving inconsistent results for the same logical operation): **fix it in the same wave** and update the test to match the fixed behaviour. Group with the surrounding test work in a single commit — don't split into micro-commits.
- **Ambiguous** (might be intentional, might be subtle): **ask the user**. State the observed behaviour, the alternative spec, and which one you'd guess. Don't pick on their behalf.

Either way, the finding is the value — pin it before moving on.

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

### `term` — ☑  ·  86.8% → **89.8%** (target ≥90%, very close)

1 · ☑ Read   2 · ☑ Audit   3 · ☑ Plan   4 · ☑ Apply   5 · n/a   6 · ☑ Fill   7 · ☑ Verify

**Outcome:** 69 → 95 tests. Audit found 2 weak asserts in `flush` (strengthened) and no rot elsewhere. Filled SGR encoding, wide-char behaviour, bounds, diff-over-style, `line` width/empty, `surface` accessors + `paint_rect`, `geometry` Rect ops.

**Bugs found and fixed (spec-first surfaced):**
- `Grid::put_str` didn't mark wide-char continuation cells, while `set`/`put_char` did. With a non-empty prev frame at the continuation slot, `flush_diff` would emit a spurious update that overwrites the wide char's right half on the terminal.
- `GridSlice::put_str` advanced col by 1 per char regardless of width — wide chars overlapped the next char.

Both fixed by routing through `set`, which already handles continuation marking. Regression test in `grid::tests::diff_does_not_emit_update_for_cell_under_a_wide_char`.

**Wrong-expectation findings (test updated, not a bug):** crossterm encodes named `Color::Red` as `\x1b[38;5;9m` (palette index 9), not classic SGR 31. Same for `Blue` bg. AnsiValue and Rgb match the ANSI spec. Tests now pin structure (SGR appears, distinct colors → distinct output) rather than exact bytes for named colors.

**Not done (deferred to property/fuzz phase):** proptest on `layout` sum-of-widths invariant.

---

### `buffer` — ☑  ·  76% → **88.5%** (target ≥90%, ~1.5pt off due to `buffer.rs` extmark/decoration paths)

1 · ☑ Read   2 · ☑ Audit   3 · ☑ Plan   4 · ☑ Apply   5 · ☑ Refactor   6 · ☑ Fill   7 · ☑ Verify

**Outcome:** 56 → 109 tests. Audit found no rot — 45 existing tests all kept, no renames/deletes/strengthens. Ring/core was already structurally correct (Sink trait + NullSink).

**Refactor:** extracted `osc52_payload(text: &str) -> Vec<u8>` from `Osc52Sink::write` so the OSC 52 encoder is testable without intercepting stdout.

**Fill:** spec-first tests landed across the previously-thin or untested pure modules:
- `undo.rs` 23% → 100%: save/undo/redo LIFO, save clears redo, cap evicts oldest, `Default` unbounded, clone independence.
- `text.rs` 77% → 100%: boundary primitives (`prev_/next_char_boundary`, `byte_to_cell`, `cell_to_byte`, `char_pos`, `byte_of_char`) with mid-char + past-end cases.
- `kill_ring.rs` 51% → 95%: `kill` push + rotation + `KILL_RING_MAX` cap, `yank`/`yank_pop` round-trip + cycling, `take/set/set_with_*`, `yank_tick` monotonicity.
- `attachment.rs` 54% → 98%: `get` known/unknown, unknown placeholder labels, `clear` resets, `image_blobs` content-addressed filenames + mime-derived extensions, `save_blobs`/`load_blobs` round-trip via `tempdir`, skip-existing semantics.
- `clipboard.rs` 0% → 53%: pure `osc52_payload` (envelope, base64, unicode round-trip, empty), `Clipboard` Sink dispatch + `swap_sink`.

No bugs surfaced — every spec matched.

Crate-wide left at 88.5% rather than ≥90% because `buffer.rs` still has unexercised extmark + decoration branches (treating that file as a follow-up; it's already at 86%).

---

### `edit` — ☑  ·  86.8% → **≥90%**

1 · ☑ Read   2 · ☑ Audit   3 · ☑ Plan   4 · ☑ Apply   5 · n/a   6 · ☑ Fill   7 · ☑ Verify

**Outcome:** added 43 direct tests for `motions.rs` (was 0) and 34 for `text_objects.rs` (was 0). Pinned the "logical-column" contract of `move_down/move_up` (returns positions that may land past EOL, relying on `clamp_normal` downstream). Pinned the punctuation-run grouping of `iw` against nvim's `utf_class` semantics.

**Refactor:** none — motions and text-objects are already pure.

**New feature: paragraph text object (`ip`/`ap`).** vip/vap/dip/dap/cip/cap etc. Implementation cross-checked against nvim `current_par` in `src/nvim/textobject.c`. Two integration tests in `vim.rs` (`dip_deletes_the_paragraph_around_the_cursor`, `dap_also_consumes_the_trailing_blank_lines`) exercise the end-to-end dispatch.

**Spec-first cross-check against nvim (after first pass):**
- `a"` — was missing trailing/leading whitespace. Vim doc: "Note that only the trailing white space is included." Fixed: include trailing whitespace, fall back to leading at EOL.
- Quote pair selection — was using a naive "chunks of 2" approach that diverged on cursor-between-strings and cursor-on-quote-of-non-first-pair. Rewritten to mirror nvim's `current_quote`: `find_prev_quote` then `find_next_quote` with `\` escape support.
- `ap` at EOF, cursor in trailing blank run — was extending backward to include leading paragraph. Vim returns FAIL there. Aligned: returns `None`.
- `ap` on non-blank paragraph with leading-only blanks — was unintentionally extending across to blanks via a stale "synthetic trailing line" hack. Removed the synthetic line; now correctly extends backward only when the cursor is in a non-blank paragraph with no trailing blanks (matches vim's last-resort branch in `current_par`).

**Bugs surfaced and fixed:** see "Spec-first cross-check" above. All four were genuine divergences from nvim behavior, fixed before landing tests.

**Not done (deferred to property/fuzz phase):** proptest on motions staying within line; sentence (`is`/`as`) and tag (`it`/`at`) text objects (lower-priority; smelt is not an HTML editor).

---

### `tui` — ◐  ·  34.7% → **37.5%** (in progress)

1 · ☑ Read   2 · ☑ Audit   3 · ☑ Plan   4 · ☑ Apply   5 · ◐ Refactor   6 · ◐ Fill   7 · ☐ Verify

**Per-wave progress:**

| Wave | Focus | Status | Tests added |
|------|-------|:------:|:-----------:|
| A | Pure fills: `prompt_sections`, `metrics`, `picker` | ☑ | 63 |
| B | Small extractions: `commands.rs` parse, `completer/file` transform, `instructions` renderer | ☑ | 25 |
| C | Dispatcher extractions: `keymap` (→ smelt-core), `app/mouse`, `app/cmdline_edit`, `app/agent` api-key | ☑ | 60 |
| D | `TestApp` harness for end-to-end state-transition tests | ☐ | — |

**Wave A outcome:** `prompt_sections` 0%→100%, `metrics` 0%→64%, `picker` 0%→50%. No refactor needed — pure helpers were already extractable.

**Wave B outcome:** extracted `parse_command_line` (commands.rs 0%→17%), `expand_with_parent_dirs` (file completer 0%→31%), `render_sections` (instructions 0%→57%). All refactors preserve behavior; minor backward-compat note on `!` lines + paste interaction documented in code.

**Wave C outcome:** moved keymap matcher into `smelt-core::keymap` (now reusable by a future GUI frontend). Extracted pure mouse-focus decisions (mouse.rs 0%→21%), cmdline text-edit + history-step state machine (`cmdline_edit` 0%→97%), and api-key env lookup (deduplicated two near-identical methods, gave it a resolver-indirection seam).

**Remaining 0% files (deferred — mostly ring with little extractable pure logic):**

| File | LoC | Notes |
|---|--:|---|
| `app/events.rs` (post-chord) | ~880 | Mostly key→action glue on `&mut self`; chord was the meaty piece |
| `app/agent.rs` | 700 | Turn-state mutations + async; api-key extracted, rest is shell |
| `app/engine_events.rs` | 286 | Exhaustive match on EngineEvent; each arm is pure ring |
| `app/transcript.rs` | 377 | Scroll/select methods on TuiApp |
| `app/history.rs` | 585 | Content-history rendering |
| `app/render_loop.rs` | 309 | Async lifecycle |

These need a **`TestApp` harness** (Wave D) — end-to-end behavioural tests on the assembled app catch interactions that pure unit tests can't reach.

**Wave D plan:**

| # | Item | Status |
|---|------|:------:|
| 1 | Skeleton: `feed(events)`, `state()`, `actions()` | ☐ |
| 2 | First suite: overlays, cmdline, Ctrl-C semantics | ☐ |
| 3 | Picker open → filter → select | ☐ |
| 4 | Vim mode transitions end-to-end | ☐ |

**Lower-priority within tui (do after dispatchers):**
- `input/buffer.rs` (34%) — make `PromptCtx<'_>` constructible from in-memory state.
- `content/transcript_parsers/tools.rs` (26%) — fill parser branches.

**Follow-ups recorded:**
- Build a real Rust-side `KeymapRegistry` (trie/sorted) so the matcher queries Rust state directly instead of round-tripping through `mlua` per decay step. Today's `ChordOracle` adapter goes away. Lua handlers become opaque `HandlerRef`s the matcher returns on `Consumed`. Estimated: medium refactor, touches `tui/src/lua/keymap.rs` + every keymap-related Lua API. Driver: chord-heavy workflows hit the per-key mlua cost; testability also wins.

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

**Active crate:** `tui` — Waves A/B/C done. **Wave D (TestApp harness) is next.**

Most remaining 0% files are imperative shell with thin pure decisions already extracted. Further line-coverage lift requires end-to-end behavioural tests on an assembled app: feed events in, assert state/actions out. Skeleton + first suite (overlays open/close, `:` opens cmdline, Ctrl-C cancel) sets the foundation; later suites cover picker filter/select and vim-mode transitions.
