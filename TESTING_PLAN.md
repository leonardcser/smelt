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

### `tui` — ☑  ·  34.7% → **≥40%** (Wave D done; further lift comes via fuzzing or `core`-side coverage)

1 · ☑ Read   2 · ☑ Audit   3 · ☑ Plan   4 · ☑ Apply   5 · ☑ Refactor   6 · ☑ Fill   7 · ☑ Verify

**Per-wave progress:**

| Wave | Focus | Status | Tests added |
|------|-------|:------:|:-----------:|
| A | Pure fills: `prompt_sections`, `metrics`, `picker` | ☑ | 63 |
| B | Small extractions: `commands.rs` parse, `completer/file` transform, `instructions` renderer | ☑ | 25 |
| C | Dispatcher extractions: `keymap` (→ smelt-core), `app/mouse`, `app/cmdline_edit`, `app/agent` api-key | ☑ | 60 |
| D | `TestApp` harness for end-to-end state-transition tests | ☑ | 27 |

**Wave A outcome:** `prompt_sections` 0%→100%, `metrics` 0%→64%, `picker` 0%→50%. No refactor needed — pure helpers were already extractable.

**Wave B outcome:** extracted `parse_command_line` (commands.rs 0%→17%), `expand_with_parent_dirs` (file completer 0%→31%), `render_sections` (instructions 0%→57%). All refactors preserve behavior; minor backward-compat note on `!` lines + paste interaction documented in code.

**Wave C outcome:** moved keymap matcher into `smelt-core::keymap` (now reusable by a future GUI frontend). Extracted pure mouse-focus decisions (mouse.rs 0%→21%), cmdline text-edit + history-step state machine (`cmdline_edit` 0%→97%), and api-key env lookup (deduplicated two near-identical methods, gave it a resolver-indirection seam).

**Wave D outcome:** built a `TestApp` harness around `TuiApp` that takes a `SourceEvent` stream (`Term`/`Engine`/`Tick`) and returns a structured `Action` log plus snapshots — same input/output shape the eventual fuzz target will use (per `FUZZING_PLAN.md`), so suites survive the DST migration. 27 tests across four suites: Ctrl-C semantics, cmdline open/close + `:quit`, picker open/filter/select, vim mode transitions through the real chord matcher. Side effects are contained by pointing `$HOME`/XDG at a process-wide tempdir.

**Refactors driven out by the harness build (same wave):**
- `TuiApp::new` took 18 positional args, mostly unpacking what was already an `AppConfig`. Now takes the `AppConfig` directly (7 args total). Cache merging (mode + reasoning-effort fallback) moved to the startup site in `main.rs` where it belongs. `app.core.config.model_config = ...` post-construction patch is gone.
- `Timers` and `pending_dialogs` were loop-locals threaded as `&mut` through 6 dispatcher methods. They are app state, not loop state — hoisted onto `TuiApp`. `dispatch_terminal_event`/`dispatch_common`/`handle_event_idle`/`handle_event_running`/`handle_pane_chord`/`dispatch_control` each lost a parameter; `dispatch_control` lost two extras. Main loop body shrank ~12 lines.

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
| 1 | Skeleton: `feed(events)`, `state()`, `actions()` | ☑ |
| 2 | First suite: overlays, cmdline, Ctrl-C semantics | ☑ |
| 3 | Picker open → filter → select | ☑ |
| 4 | Vim mode transitions end-to-end | ☑ |

**Lower-priority within tui (do after dispatchers):**
- `input/buffer.rs` (34%) — make `PromptCtx<'_>` constructible from in-memory state.
- `content/transcript_parsers/tools.rs` (26%) — fill parser branches.

**Follow-ups recorded:**
- Build a real Rust-side `KeymapRegistry` (trie/sorted) so the matcher queries Rust state directly instead of round-tripping through `mlua` per decay step. Today's `ChordOracle` adapter goes away. Lua handlers become opaque `HandlerRef`s the matcher returns on `Consumed`. Estimated: medium refactor, touches `tui/src/lua/keymap.rs` + every keymap-related Lua API. Driver: chord-heavy workflows hit the per-key mlua cost; testability also wins.

---

### `core` — ☑  ·  50.5% → **73.0%** (Waves A/B/C done; target ≥70% achieved)

1 · ☑ Read   2 · ☑ Audit   3 · ☑ Plan   4 · ☑ Apply (Wave A)   5 · ☑ Refactor (Wave B)   6 · ☑ Fill (Wave C)   7 · ☑ Verify

**Wave A outcome:** filled the pure-core gap across markdown rendering and small helpers.

| #  | Module                          | Before | After  |
|----|---------------------------------|-------:|-------:|
| A1 | `notebook.rs`                   |    13% |   64%  |
| A2 | `html.rs`                       |    23% |   94%  |
| A3 | `content/highlight/diff.rs`     |     0% |   99%  |
| A4 | `content/highlight/inline.rs`   |    43% |   95%  |
| A5 | `content/builder.rs`            |    63% |   98%  |
| A6 | `content/selection.rs`          |     0% |  100%  |
| A6 | `content/transcript.rs`         |     0% |   95%  |
| A6 | `content/block_layout.rs`       |     0% |  100%  |
| A6 | `content/context.rs`            |     0% |  100%  |
| A6 | `fuzzy/score.rs`                |     0% |  100%  |

**Wave B outcome:** structural + behavioral tests for ring-state and rendering modules; no clock seam needed.

| #  | Module                          | Before | After  |
|----|---------------------------------|-------:|-------:|
| B1 | `working.rs`                    |     0% |  100%  |
| B2 | `transcript_model.rs`           |    53% |   99%  |
| B3 | `content/highlight/syntax.rs`   |    47% |   99%  |

**Wave C outcome:** extracted pure formatter from `mcp::call_tool`; backfilled pure helpers and registry surfaces across the remaining rings; audited 158 existing permissions tests as descriptive/behavioral (no rewrite needed). Skipped `engine_client` (needs `EngineHandle` mock, low leverage). `mcp/dispatcher.rs` is pure ring shell — not testable without integration.

| #  | Module                          | Before | After  |
|----|---------------------------------|-------:|-------:|
| C1 | `mcp/mod.rs` (extract+fill)     |     0% |   53%  |
| C2 | `confirms.rs`                   |     0% |  100%  |
| C2 | `history.rs`                    |     0% |   92%  |
| C2 | `session.rs`                    |    12% |   72%  |
| C2 | `process.rs`                    |    25% |   64%  |
| C3 | `permissions/approvals.rs`      |    47% |   98%  |
| C3 | `permissions/rules.rs`          |    47% |   63%  |
| C3 | `permissions/store.rs`          |    38% |   55%  |

Bugs surfaced during testing:
- `<title>` HTML5 RCDATA semantics — nested tags preserved literally (wrong-expectation finding, not a code bug).
- `Confirms` had a derived `Default` whose `is_clear_flag=false` was inconsistent with `new()`'s `true`. No callers used it. Removed.
- `RuntimeApprovals::add_session_tool` / `add_workspace_tool`: first call on a fresh entry discards patterns and falls through to blanket approval, because empty-Vec is the blanket signal in `is_approved`. Documented in tests; not fixed because existing tests rely on the behavior.

---

### `engine` — ☑  ·  40.1% → **65.9%** (target ≥75%; remaining gap is OAuth/async-ring code that resists unit testing)

1 · ☑ Read   2 · ☑ Audit   3 · ☑ Plan   4 · ☑ Apply   5 · ☑ Refactor   6 · ☑ Fill   7 · ☑ Verify

**Provider extraction (substep 5):**
Replicated the shape of `provider/extract.rs` (95% cov) on each provider — pulled `build_body`, `parse_response`, `apply_sse_event`/`StreamState::finalize` into pure free functions.

| # | Module                              | LoC   | Cov before | Cov after | Status |
|---|-------------------------------------|------:|-----------:|----------:|:------:|
| 1 | `provider/sse.rs`                   |    45 |        ~0% |       60% |   ☑    |
| 2 | `provider/auth_storage.rs`          |    89 |         0% |       72% |   ☑    |
| 3 | `provider/openai.rs`                |   342 |         0% |       86% |   ☑    |
| 4 | `provider/anthropic.rs`             |   285 |         0% |       86% |   ☑    |
| 5 | `provider/chat_completions.rs`      |   210 |         0% |       83% |   ☑    |
| 6 | `provider/codex.rs`                 |   653 |         0% |       39% |   ☑    |
| 7 | `provider/copilot.rs`               |   591 |       small|       34% |   ☑    |
| 8 | `provider/mod.rs`                   | 1 047 |         0% |       65% |   ☑    |

**Companion non-provider modules tested in the same pass:**

| Module          | LoC | Cov before | Cov after |
|-----------------|----:|-----------:|----------:|
| `cancel.rs`     |  53 |         0% |      100% |
| `compact.rs`    | 500 |        44% |       80% |
| `image.rs`      | 107 |         0% |       99% |
| `lib.rs`        | 229 |         0% |       95% |
| `log.rs`        | 116 |        74% |       91% |
| `paths.rs`      | 156 |        51% |       97% |
| `pricing.rs`    | 450 |         0% |       79% |
| `skills.rs`     | 232 |        34% |       94% |
| `tools/mod.rs`  | 105 |         0% |      100% |
| `trim.rs`       |  27 |        44% |      100% |
| `agent.rs`      |1764 |         0% |       13% |

**Refactors driven out by the harness build (same wave):**

- `provider/sse.rs`: extracted `drain_sse_events(buf: &mut String) -> Vec<Value>` from inside `read_events`'s async ring so the pure SSE drainer can be tested independently from a `reqwest::Response` stream.
- `provider/auth_storage.rs`: pulled `write_secure(path, json)` free fn out of `CredStore::file_save` so the disk-write + 0600 perm path is testable without going through the keyring side.
- `provider/openai.rs`, `provider/anthropic.rs`, `provider/chat_completions.rs`: each `read_stream` async ring was split into `StreamState { content, reasoning, tool_calls, usage, error? }` + `apply_sse_event(&mut state, ev, on_delta)` + `state.finalize()`. The async ring is now a 3-line shell that loops over events and forwards them to the pure step function.
- `provider/copilot.rs`: extracted `parse_models_response(&Value) -> Option<Vec<CopilotModel>>` from `fetch_available_models` so the filter/sort/dedup logic (chat-only capability, model_picker_enabled, context window, max_output_tokens) is testable without an HTTP round-trip.

**Wave outcomes:**

500 unit tests added across the engine crate. Provider rings (anthropic/openai/chat_completions) all land in the 83–86% range — the residual gap is the async `read_events` shell itself. `provider/mod.rs` hit 65% via pure helpers (`from_http`, `parse_resets_at`, `parse_retry_from_body`, `apply_response_format`, `slugify`, `normalize_short`, `parse_title_and_slug`, `messages_have_images`, `ProviderKind::*`, `sanitize_tool_call_arguments`); the remaining 35% is the `chat()` retry/auth-refresh ring.

**Why the engine didn't reach 75%:** the residual uncovered surface is dominated by `agent.rs` (1346/1552 lines, 13%) — the async engine task loop, turn dispatch, tool execution, streaming partial-result handling, and command-channel orchestration. This is genuinely ring-bound: every method either drives the LLM stream, the tool dispatcher, or the engine event channel. Unit-testable extractions were lifted (`next_request_id`, `build_provider*`, `send_usage`, `PricingContext`); the rest needs an integration harness (mocked `Provider`/`ToolDispatcher`) which is out of scope for the unit-test wave. The OAuth flows in `codex.rs` and `copilot.rs` are the other major drag — their `browser_login`, `device_code_login`, `exchange_code`, `refresh_tokens`, `fetch_models` paths all hit real network endpoints and OS keyring; we covered all of their pure helpers (`needs_refresh`, `parse_jwt_claims`, `extract_account_id`, `build_authorize_url`, `classify_refresh_error`, `parse_models_response`, `base_url_from_token`, etc.) and accepted the rest.

**Fuzz target (substep 6):** `engine::sse` — adversarial bytes, no panic, no infinite loop. **Deferred** (FUZZING_PLAN.md phase).

---

## Workspace-level items (after crate work)

| # | Item                                | Status | Notes |
|---|-------------------------------------|:------:|-------|
| 1 | CI: `cargo llvm-cov nextest --workspace --fail-under-lines 52` | ☐ | Ratchet floor after each crate completes. |
| 2 | Storybook snapshot diffs in CI      |  ☐    | Already works locally. |
| 3 | Fuzz: transcript / markdown parsers |  ☐    | After `core` is done. |
| 4 | Property tests round (cross-crate)  |  ☐    | After foundations are clean. |

## Now

**Active crate:** `engine` — done (40.1% → 65.9%; 500 tests added). Target was ≥75%; remaining gap is `agent.rs` (async ring) + OAuth flows in `codex.rs`/`copilot.rs`, both of which need an integration harness rather than more unit tests. Next: workspace-level items (CI floor, fuzz targets).

The `TestApp` harness lives in `crates/tui/src/app/test_harness.rs` and is the wedge for any further interactive testing in `tui`. When `FUZZING_PLAN.md`'s Phase 1+ lands, the same `SourceEvent` enum drives both the harness and the fuzz target — no rewrite.
