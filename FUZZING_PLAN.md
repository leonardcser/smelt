# Fuzzing Plan

Deterministic simulation testing (DST) for smelt. End state: fuzz finds crashes/hangs/invariant violations against a pure `step()` function; failures serialize to a `Scenario` file; a replay binary opens a real terminal and walks through the scenario step-by-step.

Principles in [`TESTING.md`](./TESTING.md). Rolling test work in [`TESTING_PLAN.md`](./TESTING_PLAN.md).

## Architecture

Three pillars:

1. **Pure `step()`** — `step(&mut State, SourceEvent) -> Vec<Effect>`. No I/O.
2. **Effect indirection** — every side effect goes through a trait. Prod impl does real I/O; sim impl returns scripted responses.
3. **Single event queue** — one `SourceEvent` enum unifies keys, mouse, engine, Lua, timers, exec, signals. Prod source multiplexes real streams; sim source yields from a `Vec`.

```
SourceEvent ─→ step(state, ev) ─→ (state', Vec<Effect>)
                                         │
                                         ▼
                               Effects trait
                                 prod: tokio HTTP, fs, crossterm
                                 sim:  scripted responses, virtual fs/clock
```

A `Scenario` is `(initial_state, Vec<SourceEvent>, Vec<scripted_effect_response>)`. Same code path drives prod and sim — sim is only the trait impls.

## Phases

### Phase 0 — Determinism audit

Find every nondeterminism source and list injection sites. No code changes; just a punch list.

- [ ] Audit `Instant::now`, `SystemTime::now`, `tokio::time::{sleep, interval, Instant}`
- [ ] Audit `rand::*`, `Uuid::new_v4`, any other RNG
- [ ] Audit `HashMap`/`HashSet` iteration in decision-bearing code (vs. write-only collections)
- [ ] Audit `std::env::var`, `std::env::current_dir`, process info
- [ ] Audit filesystem reads in hot paths (config, transcripts, attachments)
- [ ] Audit channel/select usage for ordering surprises
- [ ] Audit thread spawns and `tokio::spawn`

Output: one consolidated punch list with `file:line` per site, grouped by category.

### Phase 1 — Inject abstractions

Smallest possible PR per injection. Tests prove behavior unchanged.

- [ ] `Clock` trait + `VirtualClock` impl. Replace audited `now()` calls.
- [ ] `Rng` trait + seeded impl. Replace audited rand calls.
- [ ] Swap decision-bearing `HashMap`s to `BTreeMap` (or seeded `RandomState`).
- [ ] Snapshot `env`/`cwd` once at startup; pass through state.

### Phase 2 — Event unification

- [x] `SourceEvent` enum covering `Term`, `Engine`, `LuaWakeup`, `ExecOutput`, `ExecDone`, `Tick`, `Resize`. Lives in `tui::event_source`; the harness re-exports it so production and tests share one shape.
- [x] `EventSource` trait + `ScriptedSource` impl (scripted-vec driver) for tests / replay binary.
- [ ] Funnel the production main loop through `EventSource::next()`. The current `tokio::select!` in `tui/src/app.rs` produces and immediately consumes each arm inline; the migration is to lift each arm into a `SourceEvent` emit, drop a single `dispatch(event)` step at the bottom, and have prod's `LiveSource` wrap the existing `select!`. Behavior-preserving; touches the main loop and is risky enough to land as its own PR.

### Phase 3 — Effect indirection

- [ ] Inventory every effect the TUI performs (render, fs write, HTTP send, exec spawn, clipboard, OSC52, …).
- [ ] `Effect` enum + `Effects` trait.
- [ ] Prod impl is the current code, lifted behind the trait.
- [ ] Mock impl with scripted responses keyed by effect ordinal.

### Phase 4 — Pure `step()`

- [ ] Bridge dispatcher extractions from `TESTING_PLAN.md` (`route_cmdline`, `route_content_keys`, `picker::route`, `route_mouse`, top-level `route`) into one `step(&mut State, SourceEvent) -> Vec<Effect>`.
- [ ] Prod loop becomes: `let ev = source.next().await; let fx = step(&mut state, ev); effects.apply(fx); render();`

### Phase 5 — Scenario + fuzz target

- [ ] `Scenario { initial: InitialState, events: Vec<SourceEvent>, responses: Vec<EffectResponse> }` with serde.
- [ ] JSON for archives, postcard for the libFuzzer corpus; converter both ways.
- [ ] `fuzz/` crate with `cargo-fuzz`. Target generates `Scenario` via `Arbitrary`, runs `step()` in a loop, asserts invariants.
- [ ] State invariants: cursor on UTF-8 boundary, cursor in `0..=source.len()`, viewport bounds, grid width×height, wide-char continuation, undo cap, picker index in range. Step budget per event (default 1000).
- [ ] Resource invariants — fail the scenario when smelt misbehaves at the allocator level, not just at the state level:
  - Per-event allocation budget. **Allocator infra already shipped**: `smelt_perf::alloc::Counting` is a `GlobalAlloc` shim tracking `alloc_count`, `bytes_allocated`, `current_bytes`, `peak_bytes` process-wide (atomic counters) plus per-thread TLS totals gated by `alloc::enable()`. `tui` lib tests install it via `#[cfg(test)] #[global_allocator]`; `TestApp::feed_one` snapshots `thread_snapshot()` before/after each event into `AllocDelta`; `TestApp::feed_one_within_budget(ev, AllocBudget)` asserts the delta stays under the cap. Default budget: `max_allocs = 10_000`, `max_bytes = 4 MiB`. Observed steady-state: ~6 allocs / 100 bytes per keystroke — the budget is a smoke alarm, not a tight bound. The fuzz target (Phase 5) calls `feed_one_within_budget` directly.
  - Steady-state leak detector. Track `bytes_in_use` and `alloc_count` after each scenario completes, with two checks: (1) running-max bytes within a scenario must not exceed `MAX_RSS_BUDGET` (default: 64 MiB); (2) across `N` repeated identical scenarios (default `N=5`), peak `bytes_in_use` must not grow monotonically — a strict-monotone increase of more than 5% scenario-over-scenario is a leak signal. Static interners (`HlGroupRegistry`, `NamespaceRegistry`) need `reset_for_test` hooks called between scenarios for this to be sound (already shipped per PR #3).
  - Static-state leak invariant. Snapshot `theme::HlGroupRegistry::len()` and `buffer::NamespaceRegistry::len()` before scenario `N=1` and after `N=5`; assert delta is zero. Same for the `NEXT_*` atomic counters listed in the Phase 0 audit.
  - Honor the libFuzzer `-rss_limit_mb` flag so the harness fails fast on raw RSS blow-ups in addition to our own counters.
- [ ] Seed corpus from existing storybook stories.

### Phase 6 — Replay binary

- [ ] `smelt-replay scenario.json` — plays at chosen rate, prints crash + step on failure.
- [ ] `--step` — frame-by-frame: space=next, `r`=rewind, `s`=dump state diff to stderr, `b N`=break at step N.
- [ ] `--bisect` — binary-search the first step where an invariant breaks.
- [ ] State snapshots: ring buffer of last K, `Clone`-based.

### Phase 7 — CI

- [ ] PR: 60s fuzz budget per target.
- [ ] Nightly: long fuzz with persistent corpus stored as artifact.
- [ ] Crash regressions land as deterministic replay tests in `tui/tests/regressions/`.

## Open questions

1. Scenario format: JSON for replay, postcard for fuzz corpus — confirm.
2. Step budget: `≤1000` dispatcher iterations per `SourceEvent`. Confirm number.
3. Single-threaded `current_thread` tokio runtime in sim (with `tokio::time::pause()`) — confirm.
4. Should replay drive a live prod instance against real APIs? Defer unless requested.

## Status

**Active phase:** Phase 1 — inject abstractions. Phase 0 (audit) complete; results below.

### Phase 1 progress

| # | Item | Status |
|---|---|---|
| 1 | `biased;` on 6 unbiased selects | ☑ |
| 2 | Sort 4 HashMap-derived `Vec`s | ☑ |
| 3 | `reset_for_test` hooks on the two unbounded-growth interners (`style/theme`, `buffer`) | ☑ |
| 4 | `Clock` trait + `RealClock` + `VirtualClock` impls plumbed through `Core`, `EngineConfig`, `Provider`, aux builders, `Turn`, `ProcessRegistry`, `Timers`, `WorkingState`, lua sleep/tasks, tui chord/esc/keypress timers, harness `Tick` | ☑ |
| 4-state | `WorkingState` and `Timers` own `Arc<dyn Clock>` (no per-call `now: Instant`); determinism tests use `VirtualClock` + `advance()` fixture | ☑ |
| 4-yank | `buffer::KillRing::mark_yanked` + `edit::VimContext` + `edit::Window::handle_key`/`EventCtx` + tui `display_selection_range` thread `now: Instant` end-to-end so yank-flash window observes the host clock | ☑ |
| 4-frame | tui `app.rs` per-frame `last_frame` + `yank_flash_active` read from `core.clock` | ☑ |
| 4-click | `edit::Ui::resolve_split_mouse` / `record_click` take `now: Instant`; tui mouse handler feeds `core.clock` so double-click counting is deterministic | ☑ |
| 4-harness | End-to-end harness tests proving clock plumbing via `Tick`: `vim_yy_yank_flash_expires_after_tick` (kill-ring chain), `ctrl_w_pane_chord_expires_after_tick_past_window` (pane chord), `record_click_resets_after_400ms_gap` (double-click gap) | ☑ |
| 4-tail | Cosmetic-only `Instant::now()` left in prod: `edit::lib.rs:810` (`drag_autoscroll_since` ramp), `tui/input/mod.rs::ESC chord` ramp timestamps. No state decision depends on them — defer | ◐ low |
| 4-defer | Deferred to Effects (Phase 3) — these guard real I/O, never run in sim: `grep.rs` rg subprocess deadline, `process.rs::Output::run` blocking deadline, `log::entry`, `pricing::now_secs`, `messages.rs` ts, `session.rs::now_ms`, `http/cache.rs`, `provider/mod.rs::unix_now`, content `EPOCH` `OnceLock`, OAuth tokio deadlines | ☐ |
| 5 | `engine::env::RuntimeEnv` snapshots pid, home, xdg dirs, cwd, parallelism. Plumbed through `Core` + `TuiApp` + `HeadlessApp`; `Session::new`/`fork` take `pid` + `cwd` explicitly; session id no longer mixes stack-address entropy. Remaining direct env reads in the audit list migrate per call site as they're touched. | ◐ partial |
| 5-tail | Migrate audit-listed direct readers to `core.env.*` opportunistically: `core/session.rs::now_ms` (deferred), `engine/paths.rs::{home_dir, config_dir, …}`, `core/state.rs:145 xdg`, `core/lua/runtime.rs::xdg`, `tui/sleep_inhibit.rs::pid`, `core/lua/api/os.rs::{getenv, set_current_dir}`, `tui/app/events.rs::{VISUAL, EDITOR}`, `tui/theme.rs::{COLORFGBG, TERM}`, `buffer/clipboard.rs::WAYLAND_DISPLAY` | ☐ |
| 6 | Single-threaded sim runtime + `available_parallelism = 1` | ☐ |

**Scope deviation on item 3:** narrowed to the unbounded-growth interners (`HlGroupRegistry`, anon-styles maps, `NamespaceRegistry`). `EPOCH: OnceLock<Instant>` and `LOG_PATH: OnceLock<PathBuf>` deferred — they fix themselves naturally when item 4 routes `spinner_glyph` through the clock and when the `Effects` trait (Phase 3) routes log writes through an effect. `headless.rs` color caches stick to first-scenario value but don't grow — low priority.

### Phase 2 progress (parallel work on `next`)

`crates/tui/src/app/test_harness.rs` landed (`6b7da688`). Already pins the `SourceEvent` shape we need: `Term(crossterm::Event)`, `Engine(EngineEvent)`, `Tick(u64)`. The `Tick` arm is currently a no-op explicitly waiting on this plan's Phase 1 clock work (`test_harness.rs:193`). This means the Clock trait must:
- expose `now() -> Instant` and `system_now() -> SystemTime` for production reads,
- expose `advance(Duration)` on the sim impl so the `Tick(ms)` dispatch can do `clock.advance(Duration::from_millis(ms))`.

### Clock trait shape (decided before PR #4)

Two narrow traits + a combined alias, all in `crates/engine/src/clock.rs` (engine is the lowest workspace crate with real time needs; `core` and `tui` depend on it). Method names follow the return type (`instant_now` / `system_now`) so call sites holding `&dyn Clock` don't need UFCS to disambiguate:

```rust
pub trait MonoClock: Send + Sync {
    fn instant_now(&self) -> std::time::Instant;
}

pub trait WallClock: Send + Sync {
    fn system_now(&self) -> std::time::SystemTime;
}

pub trait Clock: MonoClock + WallClock {}
impl<T: MonoClock + WallClock + ?Sized> Clock for T {}
```

Rationale for split:
- `Instant` and `SystemTime` have different semantics (monotonic vs wall, never-jumps vs can-jump). Most call sites need exactly one. Forcing every mock to implement both is friction.
- The combined `Clock` alias is for the few sites that genuinely need both (`engine/log.rs::entry` records wall-clock timestamps but compares monotonic for log-level gating; agent turn timing uses Instant only).

Two impls ship with the trait:

```rust
pub struct RealClock;  // delegates to Instant::now / SystemTime::now

pub struct VirtualClock { /* Mutex<Instant>, Mutex<SystemTime> */ }
impl VirtualClock {
    pub fn new(start_mono: Instant, start_wall: SystemTime) -> Self { ... }
    pub fn advance(&self, dur: Duration) { ... }  // bumps both
}
```

Tokio integration: most `tokio::time::Instant::now()` sites (`provider/codex.rs`, `provider/copilot.rs`) can be replaced with `std::time::Instant::now()` (they interop fine for deadline arithmetic). Sim runtime uses `tokio::time::pause()` + `advance` so `tokio::time::sleep`/`timeout` advance virtual time; this is orthogonal to our trait.

PR #4 lands the trait + both impls + **one** adopter (likely `engine/log.rs::entry()` since it uses both Instant and SystemTime — best stress-test of the combined trait). Remaining ~46 sites migrate per-crate in follow-up PRs.

---

## Phase 0 results — determinism punch list

Audited by hand via direct grep + source reads across the workspace. All counts below verified; line numbers approximate but file paths are real. Subagent runs missed entire categories (canonicalize, global interners, several unbiased selects, all of `core/working.rs`, `core/timers.rs`, `core/lua/task.rs`, `core/grep.rs`); this section supersedes them.

### Unbiased `tokio::select!` — verified

Eleven `tokio::select!` sites in prod. Seven are problematic:

| Site | Arms | `biased;`? | Action |
|---|---|---|---|
| `crates/core/src/process.rs:166` | cancel / stdout / stderr / deadline | **yes** | keep |
| `crates/core/src/process.rs:330` | stdout / stderr / kill | **no** | add `biased;` (kill first) |
| `crates/core/src/headless_app.rs:88` | engine.recv / cancel.notified | **no** | add `biased;` (cancel first) |
| `crates/tui/src/commands.rs:221` | kill / stdout / stderr | **yes** | keep |
| `crates/tui/src/app.rs:989` | term / engine / lua / exec / sleep / sigwinch | **yes** | keep |
| `crates/engine/src/provider/sse.rs:14` | cancel / chunk | **no** | add `biased;` (cancel first) |
| `crates/engine/src/provider/mod.rs:588` | cancel / req.send | **no** | add `biased;` (cancel first) |
| `crates/engine/src/agent.rs:46` | single arm (cmd_rx.recv) | n/a | keep |
| `crates/engine/src/agent.rs:1109` | futs / side_futs / cancel / cmd_rx | **no** | add `biased;` (cancel first) |
| `crates/engine/src/agent.rs:1582` | chat_future / cmd_rx | **no** | add `biased;` (chat first or by intent) |
| `src/main.rs:234` | sigint / sigterm | **no** | low priority; either is fine |

**Net: 6 selects need `biased;`** (counted `main.rs:234` as low-priority — both arms shut down).

### HashMap iteration (decision-bearing) — verified

Four real leaks. All other HashMaps in the workspace are lookup-only or sorted before output (e.g. `skills.rs`, `process.rs::list`, `lua/runtime.rs::list_commands_with_desc`).

| Site | Why | Severity |
|---|---|---|
| `crates/core/src/headless_app.rs:135` | `args.keys()` on real `HashMap<String,Value>` (per `protocol/event.rs:170 ToolStarted.args`) joined into a tool summary string | High |
| `crates/core/src/lua/runtime.rs:597 tool_defs()` | Returns `Vec<ToolDef>` from `handlers.keys()` iteration; consumed by `tui/commands.rs:319`, `tui/app/agent.rs:75`, `tui/lua/mod.rs:723` without sort | High |
| `crates/core/src/mcp/mod.rs:197 tool_defs()` | `connections: RwLock<HashMap<String,McpConnection>>.values().flat_map(...)` | High |
| `crates/core/src/lua/runtime.rs:436 command_names()` | Returns `Vec<String>` from `HashMap.keys()` | Low (display) |

Fix: sort each Vec before return.

### Time reads — verified (prod only, tests excluded)

**`Instant::now`** (28 prod sites; many sub-agent claims were tests):

- `core`: `grep.rs:97, 120` (grep deadline); `process.rs:67, 90, 312` (blocking deadline + registry started_at); `working.rs:79, 90, 160, 282` (turn-phase scheduling — central); `content/mod.rs:59` (spinner EPOCH `OnceLock`); `content/stream_parser.rs:371, 451` (stream timing); `timers.rs:49` (timer set); `lua/runtime.rs:586, 1060` (Lua drive); `lua/task.rs:184` (Lua sleep deadline)
- `engine`: `agent.rs:90, 945` (turn `started_at`, `tool_start`); `provider/mod.rs:550` (request_start); `provider/codex.rs:592, 597` (refresh deadline); `provider/copilot.rs:204, 212, 221` (OAuth poll deadline)
- `tui`: `app.rs:454, 975, 977` (main loop frame); `app/events.rs:243, 385, 414, 438` (key debounce, double-ESC, double-Ctrl-C); `app/pane_focus.rs:33` (pane chord); `app/transcript.rs:357, 395`, `input/mod.rs:158` (yank flash); `input/mod.rs:1132, 1153` (ESC chord)
- `edit`: `lib.rs:127, 810`
- `buffer`: `kill_ring.rs:155` (yank flash)
- `perf`: gated behind `if !enabled() return;` — **skip**

**`SystemTime::now`** (10 prod sites): `core/session.rs:159` (`now_ms`), `core/messages.rs:66`, `core/http/cache.rs:22`, `engine/log.rs:47, 91`, `engine/pricing.rs:20`, `engine/provider/mod.rs:182`, `tui/app.rs:473`, `tui/app/engine_events.rs:39`, `tui/content/transcript_parsers/thinking.rs:39`.

**`tokio::time::sleep` / `timeout`** (11 prod sites): `core/process.rs:159` (deadline), `core/process.rs:402` (poll), `core/mcp/mod.rs:124, 139, 223` (server init/list/call timeouts), `tui/app.rs:1104` (frame sleep), `engine/provider/codex.rs:193, 595`, `engine/provider/copilot.rs:224`, `engine/provider/mod.rs:605, 678` (retry backoffs).

**`std::thread::sleep`** in prod: `core/process.rs:100` (blocking poll inside `Output::run`). Others (`core/grep.rs:130`, `core/state.rs:166`, `core/timers.rs:113, 135`) are inside `#[test]` blocks — skip.

Action: introduce `Clock` trait, replace `Instant::now()` and `SystemTime::now()` with `clock.now()`; in sim, virtual time via `tokio::time::pause()` + advancing manually.

### RNG — verified

- `engine/provider/codex.rs:109, 126` — `rand::rng().fill()` for OAuth PKCE/state. User-initiated login only; downgrade.
- `core/http.rs:83 UA_COUNTER` + `random_user_agent` — deterministic counter (PCG mixing); harmless on single-threaded sim. No action.
- No `Uuid`, `ulid`, `getrandom`, `fastrand`, `nanorand` usage anywhere.
- `tempfile::*` — all in `#[cfg(test)]` except `tui/app/events.rs:566` (`edit_in_editor`, user-initiated) and `tui/input/mod.rs:1045` (`env::temp_dir().join("agent_clipboard.png")`, user-initiated paste) and `tui/lua/mod.rs:856` (test scaffolding). Downgrade.

### Filesystem — verified

- **Decision-bearing (was missed by audits): `crates/core/src/permissions/workspace.rs:35, 39, 43, 51`** — `Path::canonicalize()` inside `is_in_workspace()`. The result drives permission allow/deny. Real FS state affects gate decisions.
- `core/path.rs:45` — `fs::canonicalize` in path normalization helper.
- `core/trust.rs:130` — `fs::canonicalize` for trust hash root.
- `core/content/selection.rs:64 try_at_ref()` — `Path::exists()` per `@ref` token. Called during transcript parse (per-message ingest, not per render). Downgrade but still affects scenario replay if FS changes.
- `core/fs.rs:65, 74, 110, 220` — `fs::metadata` reads (file state cache).
- `core/lua/runtime.rs:154, 164, 189` — startup config loads.
- `core/trust.rs:37, 59, 68` — trust dir SHA at startup.
- `tui/completer/file.rs` — `read_dir` on user-initiated completion.

Action: `Fs` trait in Phase 3. Sim serves `canonicalize` from a pre-baked map.

### Env / process — verified

**Env reads** (16 prod sites):
- XDG/HOME (snapshot once): `engine/paths.rs:27, 33, 40, 47, 54` (HOME / XDG_{CONFIG,STATE,CACHE,DATA}_HOME); `core/state.rs:145` (XDG_STATE_HOME); `core/lua/runtime.rs:1226` (XDG_CONFIG_HOME).
- Color: `core/headless.rs:162, 165, 170` (NO_COLOR / TERM / FORCE_COLOR — `OnceLock` cached).
- Terminal: `tui/theme.rs:37, 57` (COLORFGBG / TERM).
- Editor: `tui/app/events.rs:562, 563` (VISUAL / EDITOR).
- Clipboard: `buffer/clipboard.rs:66, 100` (WAYLAND_DISPLAY).
- Config: `engine/lib.rs:34` (COMPACT_THRESHOLD_ENV), `engine/provider/auth_storage.rs:21`, `core/headless_app.rs:28` (api_key_env), `tui/app/agent.rs:394`, `src/startup.rs:9`.
- Lua-exposed: `core/lua/api/os.rs:20` (`getenv`).

**`dirs::home_dir`** (implicit env): `core/path.rs:91`, `core/lua/runtime.rs:1228`, `core/lua/api/os.rs:88`. Snapshot with the env.

**`current_dir`** (16 prod sites, all callable at runtime): `core/runtime.rs:56`, `core/tools.rs:8`, `core/session.rs:92`, `core/lua/api/os.rs:98`, `core/lua/runtime.rs:1204`, `core/lua/api/trust.rs:21, 39`, `tui/instructions.rs:14`, `tui/app.rs:233`, `engine/skills.rs:30`, `src/main.rs:147, 273`. `set_current_dir` exposed via `core/lua/api/os.rs:111` — Lua can mutate cwd mid-session.

**`process::id`** (7 prod sites): `core/session.rs:425` (session ID — decision-bearing), `core/state.rs:99`, `core/http/cache.rs:52`, `core/lua/api/os.rs:124`, `tui/sleep_inhibit.rs:78` (passed to `caffeinate`), `engine/log.rs:51`, `engine/pricing.rs:42`. Most are tempfile names (benign); session ID needs injection.

**`thread::available_parallelism`** (2 sites): `core/utils.rs:27` (parallel session listing), `tui/content/block_buffers.rs:78` (block buffer workers). Lock to 1 in sim.

Action: snapshot env + home + cwd + pid into `RuntimeEnv` at startup; `lua/api/os.rs:111 set_current_dir` updates the struct, not the process.

### Spawns — verified

`tokio::spawn` in prod (15 sites): `engine/lib.rs:222` (main engine task), `engine/pricing.rs:122`, `engine/agent.rs:196, 266, 318` (title/btw/ask), `core/process.rs:320` (process registry per-process readers), `core/mcp/mod.rs:77` (per-server connect), `core/lua/api/process.rs:50, 140` (Lua-initiated subprocesses), `tui/commands.rs:195` (shell escape), `src/main.rs:226, 382` (signal handler, context-window fetch), `src/startup.rs:107, 119` (Codex/Copilot model refresh).

`std::thread::spawn` in prod: `core/utils.rs:40` (parallel listing workers — gated by `available_parallelism`); `src/main.rs:190, 191` (one-time syntect + redact warm-up).

Action: single-threaded `current_thread` tokio runtime in sim → spawn order = call order.

### Static `OnceLock` / `LazyLock` state that leaks across libFuzzer iterations

This was missed entirely. libFuzzer runs many scenarios in one process; statics retain state.

| Static | Concern |
|---|---|
| `core/content/mod.rs:58 EPOCH: OnceLock<Instant>` | Spinner frame index uses `epoch.elapsed()`; scenario N sees N-1's age |
| `engine/log.rs:9 LOG_PATH: OnceLock<PathBuf>` | Filename baked from first scenario's SystemTime + pid |
| `style/theme.rs HlGroupRegistry` | Interner grows unboundedly across scenarios (`id_to_name.len()` increments forever) |
| `buffer/buffer.rs:104 NamespaceRegistry` | Same shape — namespace interner |
| `style/theme.rs` anon-styles + anon-hash maps (RwLock<HashMap>) | Grow per scenario |
| `core/headless.rs:145 COLOR_OVERRIDE, :156 RESULT` | First scenario's value sticks |
| `core/lua/shared.rs:57 wakeup_tx` | Channel tx baked once |
| Pricing/redact/regex statics | Read-only, safe |

**All static counters** (`SESSION_COUNTER`, `UA_COUNTER`, `NEXT_CELL_ID`, `NEXT_PROC_ID`, `NEXT_REQUEST_ID`, `LOG_LEVEL`, `ENABLED` flags) carry across scenarios.

Action: either expose `reset_for_test()` per static, or (cleaner) move into a `RuntimeIds`/`RuntimeRegistries` struct carried by `State`. The `theme` and `buffer` interners are the biggest concern — unbounded growth across iterations will eventually OOM the fuzzer.

### Channels

`tokio::sync::mpsc::unbounded` — engine→tui events, `lua_wakeup`, shell-exec output, process-registry completion. `oneshot` — context-window fetch. No `broadcast`/`watch` in prod. All become `SourceEvent`s in sim.

### Mutexes / atomics

Mutex acquisition order is determined on a single-threaded runtime → no action. Atomics all `Relaxed` except `engine/cancel.rs` (correct fence). Subsumed by the static-state action above.

### Joins / select_biased

`tokio::join!` / `futures::join!` / `select_biased!` — **none in prod.** No action.

### Network info / DNS / file watchers

No `to_socket_addrs`, `lookup_host`, `notify`, `WalkDir` in prod. No action.

---

Phase 2 (event unification), Phase 3 (effect indirection), and beyond build on these. Live progress is tracked in the table under the Status section above.
