# TUI runtime architecture implementation plan

Status: Implemented

## Brief

Replace broad mutable frontend roots with a small coordinator over concrete,
behavior-owning runtimes. Preserve the proven canonical storage, sparse transcript,
exact layout, memory, engine, and rendering foundations while making invalid
cross-subsystem mutations structurally difficult or impossible.

The target is not a literal collection of passive structs. State moves only when
its behavior, invariants, and lifecycle move with it.

## Added

- **Conversation invariant owner**
  - One public frontend boundary coordinates canonical session data, transcript
    state, turn lifecycle, persistence generations, resume state, and dispatch
    durability.
- **Capability-scoped Lua host**
  - Lua callbacks borrow only the concrete frontend capability required by the
    binding rather than downcasting to the complete `TuiApp`.
- **Behavior-owning prompt, overlay, and platform runtimes**
  - Each runtime keeps related state private and exposes semantic operations.
- **External-boundary regression coverage**
  - Real process, HTTP/parser, filesystem replacement, server, and terminal
    lifecycle failures receive focused tests.
- **Injected runtime paths**
  - Application and test construction use explicit home, XDG, and cwd state;
    process-global mutation remains only in subprocess boundary tests.

## Modified

| Area | Before | After |
|---|---|---|
| Main app | `TuiApp` exposes dozens of fields and behavior across many `impl` files | `TuiApp` coordinates a small set of private runtime owners |
| Conversation | Session, transcript, turn, persistence, parser, and resume state are coordinated by callers | `ConversationRuntime` owns their shared invariants and semantic operations |
| Session mutation | Generic mutation union, target router, persistence boolean, broad result bag | Direct owner methods with operation-defined durability and focused outcomes |
| Prompt | Input, history, queue, placeholder, and resize state are separate app fields | `PromptRuntime` owns prompt lifecycle and invariants |
| Overlays | Dialog, notification, cmdline, search, picker, shell, and execution state are separate | `OverlayRuntime` owns overlay lifecycle and returns concrete platform effects |
| Lua | Safe scoped borrowing still projects the whole app through `Any` | Scoped host exposes narrow concrete capability views; no `with_app` or `Any` downcast |
| Platform | Terminal, process channels, status, sleep, inspect, and context updates live on the app | `PlatformRuntime` owns resource acquisition, completion, and shutdown |
| Paths | Tests serialize HOME, XDG, and cwd mutation | Most tests construct explicit `RuntimeEnv` and run safely in parallel |
| Storybook | A second large facade can mutate `TestApp.app` internals | Storybook is a snapshot adapter over supported semantic `TestApp` operations |
| Benchmarks | Ignored manual runners live in correctness test modules | Dedicated benchmark support and runners contain manual workloads; normal tests retain correctness gates |

## Removed

- **Whole-app Lua authority**
  - Remove `LuaHost::as_any_mut`, `with_app`, `try_with_app`, and bindings that
    directly traverse `TuiApp`.
- **Generic session mutation routing**
  - Remove `SessionMutation`, target routing, irrelevant no-op arms, and caller-
    selected persistence flags.
- **Public mutable frontend fields**
  - Remove ordinary production and storybook mutation of runtime internals.
- **Process-global test setup where explicit paths suffice**
  - Remove avoidable HOME, XDG, PWD, and cwd mutation from in-process app tests.
- **Embedded ignored transcript benchmark suites**
  - Remove manual benchmark runners from correctness test modules after moving
    them behind dedicated support.

## Out of scope

- Replacing canonical per-session SQLite or its single-writer lease.
- Replacing the durable submitted-turn `CommitReceipt` barrier.
- Replacing sparse transcript records, semantic anchors, exact measurement, or
  active transcript memory budgets.
- Replacing typed engine `HostCall` interactions.
- Introducing a generic repository, global event bus, service locator, event
  sourcing system, or universal heavyweight test harness.
- Removing documented `COMPAT(...)` paths before their deletion criteria are met.
- Rewriting proven transcript rendering merely to reduce file size.

## Architectural target

```text
TuiApp
├── services: AppServices
├── conversation: ConversationRuntime
│   ├── session: SessionRuntime
│   └── transcript: TranscriptRuntime
├── prompt: PromptRuntime
├── overlays: OverlayRuntime
├── lua: LuaRuntimeHost
├── platform: PlatformRuntime
└── ui: Ui
```

The names may change when a smaller concrete design is clearer. The ownership
rules may not:

1. Runtime state is private to one owner.
2. The owner contains the behavior that maintains that state.
3. Cross-owner work returns method-specific outcomes, not generic events.
4. The coordinator applies external effects but does not reconstruct owner
   invariants.
5. Lua receives a capability projection, never the application root.
6. Persistence generation and transcript invalidation are consequences of domain
   operations, never caller-selected booleans.
7. Resource owners restore or terminate resources on every drop and panic path.

## Preserved invariants

Every phase must preserve:

- `session.db` is the canonical per-session store.
- One writer owner and stable lease exist for a writable session.
- Submitted turns use one canonical transaction and return a durable receipt
  before model dispatch.
- `catalog.db` remains the only derived persistent projection.
- Sparse resumed sessions hydrate bounded windows and preserve semantic anchors.
- Exact visible layout and sparse extent estimates remain distinct and explicit.
- Transcript and renderer caches remain bounded by measured memory budgets.
- UTF-8 mutations use `smelt_buffer::text` or attached-text APIs.
- Ordinary callbacks do not depend on ambient Lua host scope.
- Compatibility behavior remains tagged and documented until deliberately
  removed.

## Implementation stages

### Stage 0: characterize external boundaries

Before moving ownership, protect behavior whose lifecycle currently crosses
process, network, filesystem, or unsafe platform boundaries.

#### Shell panel

- Drive a real shell command through user command input.
- Verify stdout and stderr streaming.
- Verify line-cap enforcement.
- Verify exit status presentation.
- Verify close after completion.
- Verify closing a running panel terminates and reaps the child.

#### Upgrade

- Separate release parsing, update selection, download planning, staging, and
  installation.
- Download and extract into a unique staging directory.
- Validate the expected archive entry and executable before touching the current
  executable.
- Preserve executable permissions.
- Atomically replace from the same filesystem with rollback on failure.
- Clean partial downloads, extracted files, and backups on every failure path.
- Cover malformed metadata, HTTP failure, archive failure, invalid archive shape,
  replacement failure, rollback, and cleanup.

#### Inspect server

- Cover session list pagination, detail, summary, request list, and request
  payloads using canonical seeded sessions.
- Cover malformed query values and cursors, invalid IDs, missing sessions,
  unavailable storage, and shutdown/listener reuse.

#### Terminal input

- Cover EOF, partial streams, malformed streams, shutdown while blocked,
  shutdown with buffered escape bytes, writer closure, repeated spawn/drop, and
  file-descriptor cleanup.

Exit gate: focused tests pass and externally visible behavior is represented by
normal, non-ignored tests.

### Stage 1: conversation ownership

- Introduce `ConversationRuntime` as the only frontend owner of canonical
  conversation state.
- Move `Session` out of the broad shared `Core` root.
- Move `TuiSessionDocument`, `TranscriptDocument`, parser, tool drafts, committed
  transcript view, resume cache, turn lifecycle, persistence handles and epochs,
  session access, and shared-session publication behind the owner.
- Keep `SessionRuntime` and `TranscriptRuntime` private inner components when that
  reduces local complexity, but expose one facade for cross-component operations.
- Make submitted-turn dispatch a single owner operation that prepares canonical
  state, waits for its durable receipt, and only then returns a dispatch request.
- Make reset, load, resume, fork, cancel, finish, and shutdown owner operations.
- Replace direct field access with semantic methods and read-only snapshots.

Exit gate: production code cannot independently mutate session, transcript,
turn, or persistence-generation state.

### Stage 2: direct domain mutation API

- Replace the generic `SessionMutation` union with direct methods.
- Remove `SessionMutationTarget` and all irrelevant target/mutation combinations.
- Remove `persist_mutation: bool`; operation semantics decide persistence.
- Keep dirty-generation and invalidation bookkeeping private.
- Return focused outcomes only when the coordinator must perform an effect.
- Keep history rewrite primitives shared between materialized and store-backed
  sessions.
- Encode invalid operations in method types or return explicit errors; do not
  silently succeed.

Exit gate: adding a new domain operation does not require editing unrelated
mutation-routing match arms.

### Stage 3: prompt and overlay ownership

#### Prompt runtime

Move input state, history, queues, stash, placeholders, prompt-height measurement,
manual resize state, last-published text, and prompt-specific focus behavior into
one owner. Its methods take the relevant `Ui` buffer/window explicitly and keep
source installation, attachment IDs, cursors, undo, and completer state aligned.

#### Overlay runtime

Move dialogs, deferred dialogs, notifications, suspended notifications, cmdline,
search, pickers, shell panel, and overlay execution state into one owner. Methods
return concrete outcomes such as a process ID to cancel or a render request.
They do not call a generic event bus.

Exit gate: prompt and overlay state is private, and ordinary event handlers use
semantic operations rather than field mutation.

### Stage 4: capability-scoped Lua host

- Replace whole-app downcasting with one safe scoped capability root.
- Remove `Any` from the Lua host contract.
- Provide concrete session, transcript, prompt, overlay, UI, configuration,
  process, and platform capability views.
- Migrate every binding to the smallest capability it needs.
- Keep detached runtime handles for work that outlives one callback.
- Keep one scoped entry per actual Lua callback.
- Forbid long-lived Rust callbacks from depending on scoped host access.
- Add compile-time-private host fields and nested/reentrant regression tests.

Exit gate: searches find no `with_app`, `try_with_app`, `as_any_mut`, raw host
pointer, or binding-level access to `TuiApp`.

### Stage 5: platform ownership and explicit runtime paths

- Move terminal ownership, process completion channels, public status, sleep
  inhibition, inspect server, context update channels, and shutdown sequencing
  behind `PlatformRuntime`.
- Ensure drop and shutdown are idempotent and panic-safe.
- Treat cwd as runtime state and pass it explicitly to child processes.
- Read home and XDG paths from injected `RuntimeEnv`.
- Route Lua cwd changes through workspace/platform behavior.
- Stop changing process cwd during ordinary in-process tests.
- Retain the process environment guard only for true subprocess and process-global
  behavior tests.

Exit gate: ordinary app and storybook tests can run with independent runtime
paths without holding a process-global environment lock.

### Stage 6: supported semantic test driver

- Make `TestApp.app` private.
- Add semantic operations for settling Lua, starting and finishing turns, tools,
  permissions, session seeding, resume, rendering, shell lifecycle, and resource
  updates.
- Add explicit read-only snapshots and focused probes.
- Migrate storybook to those operations.
- Keep component and unit tests independent of the full harness.

Exit gate: storybook does not mutate `TuiApp`, `Core`, or runtime owner fields.

### Stage 7: benchmark separation

- Move manual transcript benchmark fixtures and runners behind dedicated benchmark
  support or an xtask-owned target.
- Expose only the minimum semantic hooks required for measurement.
- Extract correctness claims into normal tests.
- Remove ignored manual benchmark suites from app and renderer correctness modules.
- Preserve documented 50 MiB and 500 MiB memory and navigation workloads.

Exit gate: correctness test navigation contains no large ignored benchmark runner,
and benchmark commands continue to produce the documented measurements.

### Stage 8: simplify and validate

- Privatize remaining broad mutable fields.
- Remove adapters, superseded methods, duplicate state, no-op variants, and stale
  comments.
- Review every owner for behavior rather than passive storage.
- Review dependencies for unnecessary traits or generic abstractions.
- Regenerate Lua API documentation.
- Run formatting, clippy, full tests, snapshots, fuzz regression replay, coverage,
  debug and release-fast builds, static architecture searches, and transcript
  memory benchmarks.

Exit gate: all validation passes and static searches prove the removed authority
and mutation paths cannot be used.

## Validation commands

```bash
cargo fmt -- --check
cargo clippy --workspace --all-targets --features smelt-tui/harness -- -D warnings
cargo nextest run --workspace --features smelt-tui/harness
cargo llvm-cov nextest --workspace --features smelt-tui/harness --fail-under-lines 80
cargo xtask gen-lua-docs
cargo build
cargo build --profile release-fast --bin smelt
cargo test -p smelt-tui --test storybook_main --features harness app_dialog_permission
```

Also run committed fuzz regression seeds, `git diff --check`, static searches for
removed host and mutation APIs, and the documented transcript memory workloads.

## Completion record

Update this section after every stage with the exact implementation, deliberate
changes from the proposed shape, tests added, and validation evidence. Mark the
plan implemented only when all worthwhile stages are complete or a simpler design
has made a listed stage unnecessary while preserving stronger invariants.

### Stage 0 - complete

- Shell commands run as isolated session and process-group leaders. Closing a
  running panel kills the process group, waits for the child, and reports status
  130. Five focused tests cover real stdout and stderr streaming, retained-line
  limits, exit status, panel lifecycle, cancellation, descendant termination, and
  reaping.
- Stable upgrade checks reject HTTP failures, malformed JSON, invalid versions,
  drafts, malformed tags, and release sets without a usable candidate. Asset
  planning, download and extraction orchestration, staging, candidate validation,
  and installation are separate test seams.
- Stable release assets download and extract in a unique directory adjacent to the
  executable. The candidate must be a non-empty executable regular file, so empty,
  non-executable, missing, and symlink archive entries are rejected before install.
  Partial downloads and extractions are removed on failure.
- On Unix, installation hard-links the current executable into staging and then
  atomically renames the validated candidate over the live path. An install rename
  failure leaves the live executable untouched, which removes the need for rollback
  on supported release targets. Recovery staging is preserved only when cleanup of
  a usable backup fails. The non-Unix fallback retains explicit rename rollback.
  Thirteen focused tests cover metadata and HTTP failures, planning, download and
  archive failures, invalid archive shape, permissions, atomic replacement, failed
  replacement, backup failure, staging cleanup, and recovery preservation.
- Inspect-server tests now seed canonical `session.db` stores and drive the real TCP
  server. Nine tests cover assets, stable list pagination and cursors, detail,
  summary, request lists, payloads, malformed limits and cursors, malformed request
  IDs, invalid paths, missing resources, corrupt storage, idempotent shutdown, and
  listener reuse. Client input failures return 400, missing resources return 404,
  and unavailable canonical storage returns 500.
- Terminal input now handles `POLLHUP` as readable EOF instead of spinning forever,
  exits on invalid or errored descriptors, and keeps shutdown idempotent. Six
  pipe-backed lifecycle tests cover complete input followed by EOF, partial escape
  EOF, malformed-byte recovery, shutdown while blocked, shutdown with buffered
  escape bytes, repeated spawn and drop, and closure of the input and both shutdown
  descriptors. The full 19-test parser and lifecycle module passes.
- Focused validation passed for all four boundaries. Workspace formatting and
  warnings-denied clippy were rerun after the stage.

### Stage 1 - complete

- `ConversationRuntime` is the sole TUI owner of the canonical `Session`,
  `TuiSessionDocument`, sparse transcript, parser, tool drafts, committed view,
  resume cache, shared-session publication, turn lifecycle, persistence actor and
  epoch, access mode, and storage policy. `Session` was removed from the shared
  `Core` root; the headless frontend now owns its session directly.
- Session and document fields are private. Production code can read immutable
  session or transcript views, while mutations, hydration, anchors, folding,
  compaction, turn transitions, persistence, publication, reset, load, resume, and
  fork behavior pass through semantic owner methods. Test-only fixture operations
  are compile-time gated and do not expose general mutable accessors.
- The simpler single facade was retained instead of adding passive inner
  `SessionRuntime` and `TranscriptRuntime` wrappers. Transcript layout and sparse
  hydration continue to be implemented by `TranscriptDocument`; the conversation
  facade coordinates those operations without duplicating layout logic.
- All TUI production and test targets compile with private owner fields. Focused
  session-document, agent, history, persistence, and headless tests pass. A PCRE
  static search for `conversation.session` or `conversation.document` not followed
  by a method call reports no direct field access.

### Stage 2 - complete

- The generic `SessionMutation` union, `SessionMutationTarget` router,
  `SessionDocument::apply`, runtime and session forwarding variants, conversion
  macros, and caller-selected `persist_mutation` flag were deleted.
- `TuiSessionDocument` now exposes category-specific history, transcript, usage,
  metadata, checkpoint, model, mode, and context operations. History rewrite
  primitives remain shared between materialized and store-backed sessions, and
  invalid block-status transitions return explicit errors.
- Dirty generations and invalidation stay private. `ChangeTracking` is document
  state selected when a document is constructed, so operation callers cannot
  choose durability. Loaded-model token-baseline reset has a dedicated
  non-persisting operation. The remaining `records_persisted` values describe
  materialization or save-plan state rather than mutation durability.
- Focused mutation-sequence and owner tests pass, and warnings-denied clippy passes
  for all TUI targets with the harness enabled. Static searches report no
  `SessionMutation`, `SessionMutationTarget`, `MutationResult`, generic apply
  router, `apply_runtime`, `apply_to_session`, or `persist_mutation` symbols.

### Stage 3 - complete

- `PromptRuntime` privately owns prompt editing state, history, bounded request and
  turn queues, last-published text, height and manual-resize lifecycle, placeholders,
  and attachment storage. Buffer and window resources remain canonical in `Ui`, and
  prompt operations receive explicit `PromptCtx` or `Window` borrows so source,
  attachment IDs, cursors, undo, Vim state, and completion state stay aligned.
- Queue interruption now uses an opaque suspended-queue value. Cancellation sees an
  intentionally empty queue, after which the owner restores the untouched remainder
  before the selected request starts. This preserves request-before-turn ordering and
  prevents callers from inspecting or reordering the suspended queue payload.
- `OverlayRuntime` privately owns execution, shell-panel lifecycle, notifications and
  suspension, cmdline history and completion, search sessions and sparse-search
  indexes, picker state, and deferred dialogs. Event, render, Lua, test, and shell
  paths use semantic operations such as execution cancellation by sink, notification
  render invalidation, search-session installation, picker projection swaps, and
  deferred-dialog replay rather than mutating app fields.
- Removed prompt and overlay fields no longer exist on `TuiApp`; static field searches
  find those names only inside their owners or unrelated canonical `Ui` identifiers.
  All TUI test targets compile. Focused event, mouse, shell, cmdline, picker, search,
  prompt, and dialog suites pass, including 29 prompt and 6 picker storybook cases,
  and warnings-denied clippy passes for every TUI target with the harness enabled.

### Stage 4 - complete

- `LuaHost` no longer uses `Any`, downcasting, or direct Core access. Core and TUI
  callbacks enter separate scoped slots; the zero-state Core bridge projects `Core`
  only for the duration of a synchronous Core callback, while private `TuiLuaHost`
  methods expose named configuration, session, transcript, prompt, overlay, UI,
  model, engine, permission, terminal, and platform operations.
- Every Lua binding was migrated away from `TuiApp`. Session metadata, bounded
  history reads, checkpoint installation, rewind, persistence retry, session
  lifecycle, workspace transitions, and sparse preview projection now use owned
  snapshots or semantic host operations. Layout-tree resolution moved out of the
  binding module and into the host-side UI coordinator.
- The temporary `with_app` and `try_with_app` escape hatches were deleted. The
  scoped host stores the exclusive app borrow privately, restores outer scopes after
  nested callbacks, reports no host outside a callback, and deliberately makes a
  second capability borrow unavailable while the first mutable borrow is active.
- Work registrations use detached weak owner tokens. UI registration removal and
  retained host-reply callbacks enqueue typed operations without ambient scoped-host
  access; each callback scope drains them in order, including during unwinding. A
  pending UI removal is drained before a later host operation, preserving remove-then-
  replace ordering within one Lua callback. Model-history replies resolve against the
  canonical conversation only after the hook has completed its mutations.
- Focused Core host, Lua, session, compaction, reload, registration, busy-token, and
  reentrant-scope tests pass. All TUI targets compile, warnings-denied clippy passes
  for TUI and Core, formatting and `git diff --check` pass, and static searches find
  no `with_app`, `try_with_app`, `as_any_mut`, binding-level `TuiApp`, `Any` in the
  Lua host, raw host pointer, or scoped-host-dependent `LuaReg` callback.

### Stage 5 - complete

- `PlatformRuntime` owns terminal focus and control, process completions, app events,
  the inspect server, sleep inhibition, public-status publication and heartbeats,
  the HTTP client, context-window refreshes, and shutdown ordering. The main loop
  waits through one typed `receive` operation. Platform and inspect-server shutdown
  and drop are idempotent, including panic paths, and stale context-window results
  are rejected by revision and target.
- Runtime resources no longer derive ordinary app behavior from process globals.
  `Core` owns a concrete `SessionStorage` and its catalog worker; Lua owns explicit
  state, cache, home, and evaluation-cwd paths; trust, workspace permissions,
  metrics, prompt inputs, instructions, skills, inspect storage, and public status
  use injected roots. App-owned session reads validate against their storage owner
  exactly once, while process-boundary free functions retain process-root
  confinement. Catalog tests now wait on the worker that accepted their writes.
- Lua cwd changes update runtime and engine state without changing process cwd or
  `PWD`. Shell execution, Lua run and streaming processes, headless commands,
  external editors, shell escape, MCP, grep, LSP, clipboard helpers, sleep
  inhibition, and opener paths receive explicit cwd. Lua process launchers read the
  Lua generation's evaluation cwd, so Host-tier headless process execution remains
  available without scoped Core access. A regression executes `pwd` from an
  injected runtime directory with no Core host.
- Runtime home is part of permission policy, including shell variable and tilde
  expansion. External-editor temporary files use the injected runtime directory.
  Notebook edits resolve and display paths from explicit cwd and home roots. HTTP
  cache files use the Lua runtime's cache root. Ordinary `TestApp` instances use
  independent storage, status, cache, shell-analysis, filesystem, notebook,
  state, and prompt-input roots without acquiring the process-environment lock.
- The final stage gate passes 1,482 TUI tests and 1,324 Core tests, plus the new
  runtime-cwd process regression. All 30 permission storybook snapshots pass.
  Warnings-denied clippy passes for all TUI, Core, and engine targets; formatting,
  `git diff --check`, and static removed-authority searches pass.

### Stage 6 - complete

- `TestApp` keeps its raw `TuiApp` crate-private so focused in-crate tests can remain
  direct without exposing mutable runtime internals to supported external drivers.
  Storybook and fuzz/replay consumers use semantic turn, Lua, tool, permission,
  model, resume, shell-output, event, and canonical session-fixture operations.
- External drivers inspect immutable window snapshots and focused selection and
  paste-readiness probes. Storybook no longer constructs `SourceEvent` or
  `SessionDb` values for ordinary scenarios and does not reach through `TestApp`
  into `TuiApp`, `Core`, conversation, prompt, overlay, or platform owners.
- The final stage gate passes all 1,482 TUI tests, including the complete storybook
  snapshot binary. All TUI targets compile and pass warnings-denied clippy with the
  harness enabled; external fuzz and replay binaries compile against the restricted
  API. Formatting, `git diff --check`, and static boundary searches pass.

### Stage 7 - complete

- Manual transcript workloads moved out of app correctness-test navigation and into
  one crate-level runner enabled only by the dedicated `transcript-bench` feature.
  Renderer-specific fixtures and measurements are feature-gated benchmark support;
  normal renderer and app correctness modules contain no ignored benchmark entry.
- `cargo xtask bench-transcript-layout` builds only the TUI library test target,
  caches its executable by package, profile, and feature set, and invokes normal
  benchmark entries with an explicit target environment. Broad feature-enabled test
  runs return immediately instead of accidentally executing large manual workloads.
  The unrelated file-search benchmark retains its existing ignored-test behavior.
- Benchmark fixtures use app-owned `SessionStorage` paths and wait on the catalog
  worker that accepted their writes. This preserves the runtime storage boundary and
  verifies the derived `catalog.db` projection reaches the canonical receipt revision.
- Fresh release active-memory gates passed for 52,451,885 bytes across 1,439 blocks
  and 524,314,464 bytes across 14,384 blocks. They retained 38,621,859 and 51,637,635
  allocator bytes, used 118,648,832 and 131,006,464 bytes of in-process RSS, kept no
  committed full-content blocks live, stayed within all cache budgets, exercised
  eviction, and reread no working-set block from SQLite. The benchmark guide records
  the complete retained-memory, hydration, dematerialization, timing, and RSS results.
- The post-separation gate passes all 1,482 TUI tests. Default-feature checking,
  warnings-denied clippy for all harness targets and the dedicated benchmark library,
  formatting, `git diff --check`, and static searches for ignored transcript runners
  in app and renderer correctness modules pass.

### Stage 8 - complete

- Remaining mutable application roots are crate-private. Production installs skills,
  MCP, and prompt inputs through `TuiAppOptions` instead of mutating a constructed
  app. Overlay search mutation is exposed through semantic owner operations, and the
  obsolete width-invalidation no-op was removed in favor of renderer cache keys and
  pointer-interaction cancellation.
- The final simplification review found no passive replacement runtimes or generic
  infrastructure to introduce. `ConversationRuntime`, `PromptRuntime`,
  `OverlayRuntime`, and `PlatformRuntime` retain the behavior that enforces their
  state invariants, while `TuiApp` remains a concrete coordinator over their effects.
- Final testing exposed one child-process leak in the original 4,776-test pass. Local
  MCP servers now run in Unix process groups and Windows Job Objects, explicitly
  cancel an installed `RunningService`, and track kill-and-wait completion even when
  cancellation happens during the initialization handshake. Focused real-process
  regressions are leak-free and prove reconciliation waits for both running and
  already-exited child cleanup.
- Fuzz regression replay also exposed whitespace-only final engine text reaching the
  transcript append boundary. The engine now treats normalized-empty final text as a
  no-op while still flushing streaming state. A normal harness regression covers empty
  and whitespace-only events, and every committed fuzz seed passes.
- The final workspace run passes 4,778 tests with 2 skipped and no leaked tests.
  Warnings-denied workspace clippy and formatting pass. Coverage passes the 80% gate
  with 84.33% regions, 83.60% functions, and 85.02% lines. All 30 targeted permission
  storybook snapshots pass, as does the complete storybook binary in the workspace
  run.
- Debug and `release-fast` smelt builds pass. All fuzz binaries compile and all 83
  committed JSON and byte-form regression seeds replay successfully. Lua generation
  writes 84 modules, 460 functions, 91 classes, and 13 aliases with generated
  navigation current.
- Static searches find no generic session mutation router, caller-selected persistence
  flag, whole-app Lua escape hatch, binding-level `TuiApp`, raw Lua host pointer,
  direct conversation field access, or ignored transcript benchmark runner. The six
  code `COMPAT(...)` IDs exactly match `docs/compat.md`, and staged and unstaged diff
  whitespace checks pass.
- Stage 7's fresh 50 MiB and 500 MiB transcript memory gates remain the final memory
  evidence because Stage 8 changed ownership surfaces, MCP process cleanup, and empty
  engine-event filtering without changing transcript storage, hydration, layout,
  rendering, or benchmark behavior.

### Stage 9 - complete

- Engine output classification is no longer duplicated ahead of transcript mutation.
  `handle_engine_event` returns an `EngineEventResult` whose
  `assistant_output_started` value comes from the branch that actually applies the
  event. Normalized-empty final text and reasoning remain transcript no-ops, while
  accepted text, reasoning, and tool output report the state transition from the
  canonical application path.
- MCP child cleanup is result-bearing. The process tracker publishes either successful
  wait completion or the concrete wait failure, `McpServer::disconnect` waits for and
  propagates that result, and reconciliation records degraded cleanup in controller
  status without waiting forever on a settled revision.
- The scoped Lua boundary is split into six concrete capability hosts:
  `RuntimeLuaHost`, `ConversationLuaHost`, `AgentLuaHost`, `PlatformLuaHost`,
  `UiLuaHost`, and `SessionLuaHost`. Bindings use only their domain accessor; no
  generic whole-app host facade, raw host pointer, `Any` downcast, or generic forwarding
  shim remains.
- Integration-style harness tests now use semantic `TestApp` actions, immutable
  snapshots, and focused read-only probes. `harness_tests`, storybook, and fuzz do not
  reach through `TestApp` to `TuiApp`; direct access remains available only to focused
  unit tests beside the owning application modules. Mutating setup and lifecycle work
  stays inside named harness operations.
- The post-reflection gate passes all 4,807 workspace tests with 2 skipped, including
  the complete storybook binary. The 33 focused MCP tests, engine normalization
  regressions, Lua capability-host tests, and 30 permission storybook scenarios pass.
  Warnings-denied workspace clippy, formatting, and diff whitespace checks pass.
  Coverage passes the 80% line gate with 84.54% regions, 83.87% functions, and 85.21%
  lines. Debug and `release-fast` builds pass, all 17 fuzz targets compile, all 88
  tracked regression seeds replay successfully, and Lua documentation generation
  remains current at 85 modules, 460 functions, 92 classes, and 13 aliases.
