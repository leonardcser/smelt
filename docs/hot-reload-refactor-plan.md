# Hot reload architecture refactor plan

## Summary

smelt currently has two related problems:

1. Lua config reload reruns config code but only partially reconciles the running app state. Providers, model lists, managed model metadata, settings side effects, and some permission consumers can remain stale.
2. Managed provider model refreshes run in the background at startup, but fresh results are written to cache only. The running app continues using the stale model list until restart.

The goal is to introduce a small, explicit runtime reconciliation pipeline. Lua reload should produce a desired state. Background refreshes, settings changes, cwd changes, and auth/model cache updates should feed the same reconciler. The reconciler should be concrete and ordered, not a broad trait framework.

## Implementation principles

This plan sets the direction, not a rigid implementation script. While implementing it, prefer the better design if the code shows that the plan is not quite right.

- Treat this as greenfield-quality architecture work. Do not bolt a narrow fix onto the current shape if the touched area wants a cleaner structure.
- Optimize for simplicity, maintainability, modularity, and correctness over short-term implementation cost.
- Consolidate the refactor around the places we touch. If providers, models, settings, permissions, MCP, tools, or modes expose duplicated or awkward state, reshape that state instead of adding another parallel path.
- Keep the final system easier to work with and less error-prone than what it replaces. Fewer sources of truth are better than more reconciliation glue.
- Re-evaluate when something does not fit. If an edge case requires special handling, first ask whether the architecture should change so the edge case becomes ordinary.
- Do not defer worthwhile cleanup only because it takes time. Defer only when the work is genuinely unrelated, risky without more information, or better handled by a clearly separate refactor.
- Stay practical about abstraction. Make modules composable and boundaries clear, but avoid generic frameworks that hide ordering or make the runtime harder to reason about.

## Design goals

- Keep interactive startup fast. Do not block the TUI on network model refreshes.
- Make hot reload reliable for providers, models, permissions, settings, modes, MCP, and tools.
- Prefer explicit ordering over generic abstractions. Reload order matters and should be readable.
- Make state resolution pure where possible and side effects centralized.
- Preserve active turns and session approvals safely.
- Make long-lived consumers observe fresh runtime state without stale captured `Arc`s.
- Keep cache files as persistence, not as the live update mechanism.

## Current architecture problems

### Lua reload is destructive before success is known

`TuiApp::bring_up_lua` clears TUI Lua resources before `LuaRuntime::reload`, and `LuaRuntime::reload` clears registries before loading bootstrap/autoload/config/plugins. A syntax or runtime error can therefore leave a partially populated Lua runtime while the previous resolved app state remains. The current path still refreshes other resources, queues MCP reconciliation, refreshes layout, and drains `ready` hooks after failure.

Even on success, `reconcile_lua_runtime_config` only handles mode cycle, active mode fallback, and permission policy rebuild. Reload is not complete runtime reconciliation.

Missing from reload reconciliation:

- `smelt.provider.register` changes
- `available_models`
- managed provider dynamic models
- active model metadata
- defaults and remember config
- settings side effects such as `restrict_to_workspace`, `worktree_root`, and `auto_reload`
- complete per-turn engine model/request config
- MCP permission freshness and latest-desired convergence
- LSP configuration/removal
- watcher roots when the committed Lua/project source set changes

### Managed model refresh discards fresh results

Startup uses cached managed provider models to build `available_models`, then spawns refresh tasks. Those tasks update cache files but do not update the running app state.

Desired behavior:

- Start with cached data immediately.
- Refresh Codex, Copilot, and Kimi Code models in the background.
- Send fresh results into the TUI event loop.
- Rebuild `available_models` in memory.
- Update active model metadata for future turns and refresh dependent context-window state.

### Permissions are rebuilt, but not globally live

Permissions are rebuilt on Lua reload, but long-lived consumers can capture stale snapshots. In particular, `McpDispatcher` stores the initial `Arc<Permissions>`, so replacing `self.core.permissions` later does not update MCP permission evaluation.

Settings that affect permissions are also not consistently propagated:

- `restrict_to_workspace`
- `worktree_root`
- workspace approval roots
- workspace permission store reloads

### Model transport is too fragmented

The app stores active model state across several fields:

- `model`
- `api_base`
- `api_key_env`
- `provider_type`
- `model_config`

The engine `SetModel` command currently sends only model/base/key/provider type, not `model_config`. This can leave pricing, max-token limits, modality support, reasoning support, and tool-calling behavior stale in the engine.

The current `AppConfig` also requires a model string. That makes a fully async startup awkward for a logged-in managed provider with an empty model cache: the app cannot represent "no model yet, fetching models" cleanly. The refactor should represent selection intent separately from `Option<ActiveModel>` so startup can proceed and a later refresh can resolve the requested model.

### Settings writes are live side effects, not desired state

`smelt.settings.foo = ...` currently mutates the live app when an app context exists. During reload, that means setting assignments can update `app.core.config.settings` directly without recording the desired value in `LuaShared.settings_overrides`. A later `lua.to_config()` snapshot can therefore miss settings declared in reloaded Lua.

The refactor should make settings declarations durable desired state first, then apply live effects through reconciliation. Runtime writes can still feel immediate, but they should update the same desired-state store used by reload.

### Runtime work is not all equally reloadable

Lua reload is unsafe while a turn, modal, or Lua callback is active, so the current code defers it. Model refresh results, settings writes, and permission-store updates are different: they do not need to wipe Lua handles and can often be reconciled while a turn is active. The new pipeline should distinguish a generation-changing Lua reload from non-destructive runtime reconciliation.

### Async reconciliation can regress to older desired state

MCP reconciliation currently spawns independent tasks. An older task can acquire the manager lock after a newer task and reinstall obsolete servers, while stale connection completions can publish old tools. Watcher setup, managed model refresh, and context-window fetches have the same general identity problem. Generation counters are not optional bookkeeping; every async owner needs latest-desired convergence.

### Startup precedence is not durable runtime input

`AppConfig` retains booleans saying some CLI flags were present, but not a complete immutable object containing all actual CLI values. Reload cannot reliably reapply model transport, provider type, sampling, reasoning, tool-calling, and typed `--set` precedence by inspecting mutable current state. Defaults and remembered state also need selection semantics rather than repeated assignment.

### Some Lua APIs escape the reload transaction

`smelt.lsp.configure` immediately changes a long-lived manager. Settings can mutate the app while Lua is still loading, and Lua tasks/watchers/UI registrations can create resources during module evaluation. A reliable failed-reload guarantee requires classifying and staging all such effects, not only snapshotting `Config` after load.

## Required invariants

These invariants are more important than the exact type names in this plan:

1. **A failed Lua reload changes no live Lua-owned behavior.** Commands, keymaps, tools, hooks, renderers, providers, settings, MCP/LSP declarations, filesystem watchers, and the current project configuration remain from the last successful generation. Failed candidates do not run `ready` hooks.
2. **There is one authoritative resolved app state.** The refactor replaces `AppConfig`; it must not add a second `RuntimeState` that mirrors and synchronizes the same fields.
3. **Precedence inputs remain inputs.** CLI values, environment-enforced values, remembered selections, auth state, cwd, and managed model results are retained in typed overlay objects. Do not infer immutable CLI values from mutable current state or retain only `cli_*_override` booleans.
4. **Resolution is pure; effects are explicit.** Given committed Lua declarations, overlays, and the previous resolved state, resolution returns the next state and an effect plan. It does not spawn tasks, mutate the engine, or update UI resources.
5. **Every asynchronous controller converges to its latest desired revision.** An older model refresh, MCP/LSP connection, watcher setup, auth result, or context-window fetch may finish, but it cannot publish obsolete state.
6. **Turn behavior has a documented snapshot boundary.** Background metadata and config refreshes affect future turns. Explicit user model/mode/reasoning actions may affect an active turn only at provider-request boundaries. Session approvals are intentionally live; static permission policy remains snapshotted for the turn.
7. **No-model is represented by `None`, never an empty string.** Pending selection intent is retained independently so session restore and managed-provider startup do not lose the requested model.
8. **Reconciliation is idempotent.** Reapplying equal inputs emits no engine commands, restarts no services, and does not rewrite recent/session state.
9. **Secrets remain at dispatch boundaries.** Resolved runtime state stores API-key environment variable names, not API-key values. Logs, diagnostics, diffs, and reconcile outcomes never contain resolved secrets.
10. **Compatibility debt is explicit.** Any temporary projection or wire compatibility path is marked `COMPAT(<id>)`, documented in `docs/compat.md`, and has a removal phase in this plan.

## Architecture options and recommendation

### Option A: extend the current post-reload patching

Keep destructive `LuaRuntime::reload`, then add more assignments to `reconcile_lua_runtime_config`.

- Advantage: smallest initial diff.
- Disadvantages: failed reload remains destructive, side effects keep escaping during load, ordering remains implicit, and each new reloadable subsystem adds another synchronization path.
- Decision: reject. It fixes individual symptoms while preserving the architecture that caused them.

### Option B: transactional Lua generation plus an explicit resolver

Treat one successful Lua load as a generation. Build and validate a candidate generation without changing the live generation, resolve its desired runtime state, then commit it at the existing safe point. Non-Lua events reuse the same pure resolver and effect applier without creating a new Lua generation.

- Advantages: preserves mature runtime behavior while establishing a real transaction boundary; supports incremental migration; keeps sequencing visible.
- Disadvantages: Lua APIs that currently mutate live app state during module evaluation must become declarations or generation-scoped effects.
- Decision: **recommended**.

### Option C: rewrite the app bootstrap/runtime owner

Build a new owner around immutable startup inputs, a replaceable Lua generation, typed overlays, one resolved snapshot, and one event-driven effect executor. Port startup, headless, and TUI onto it together.

- Advantages: cleanest final ownership and least temporary migration state.
- Disadvantages: changes startup, Lua resources, turn lifecycle, and headless behavior simultaneously; regressions are harder to isolate.
- Decision: use this as the architectural north star, not as one all-at-once change.

## Proposed architecture

Use three explicit stages:

1. **Load a candidate Lua generation** for launch, manual reload, automatic reload, cwd/project change, or trust change.
   - Evaluate bootstrap, autoload, user config, project config, and plugins into candidate-owned registries and declarations.
   - Record a `LuaLoadManifest` of the config roots and files that determined the candidate.
   - Validate and snapshot declarations once, after all chunks succeed.
   - On error, discard the candidate and preserve the live generation and resolved runtime unchanged.
2. **Resolve values without side effects.**
   - Combine the committed or candidate `LuaDesiredState` with `StartupOverrides`, cwd/project context, auth state, remembered selection intent, managed model state, and session approval state.
   - Return a complete `ResolvedRuntime` and warnings, or a validation error.
3. **Commit and apply an explicit diff.**
   - For a Lua reload, atomically replace the Lua generation and resolved runtime at the safe point.
   - For a non-destructive event, replace only changed resolved values.
   - Apply synchronous TUI effects, publish state signals, and submit the latest desired revision to asynchronous controllers.

Do not introduce a broad `HotReloadComponent` trait. Use concrete `load_candidate`, `resolve_runtime`, `diff_runtime`, and `apply_runtime` functions with readable ordering.

## Transactional Lua generation

The current reload clears TUI caches and Lua registries before load success is known. The new transaction must move the clear from "before load" to "retire old generation after candidate commit."

A tentative owner is:

```rust
struct LuaGeneration {
    id: u64,
    runtime: smelt_core::lua::LuaRuntime,
    desired: LuaDesiredState,
    manifest: LuaLoadManifest,
    // Generation-owned TUI registrations/resources that are not held by LuaRuntime.
    tui: LuaTuiGeneration,
}
```

Move truly session-long services out of `LuaShared` or inject them into each candidate through a narrow `LuaHostServices`. In particular, `LspManager` must stay stable across generation swaps even though `smelt.lsp.configure` becomes a candidate declaration. Event/wakeup sinks may be shared, but every callback/task message carries generation identity so discarded candidates and retired generations cannot resume work in the live runtime.

The implementation should first inventory every API callable during bootstrap/autoload/config evaluation and assign it one of these behaviors:

| Surface | Candidate-load behavior |
| --- | --- |
| Providers, settings, defaults, remember, modes, permission rules, tool defaults, MCP, LSP, default shell | Record declarations in candidate state only |
| Commands, keymaps, Lua tools, hooks, renderers, callbacks, transcript groups | Register against the candidate generation |
| Lua tasks, timers, `smelt.fs.watch`, async callbacks | Create paused/candidate-owned work; activate only after commit and cancel on discard |
| Named UI resources, layout registrations, and user notifications | Stage bindings/messages or make them generation-owned; do not mutate live UI before commit |
| Process/network/filesystem mutations requested during module evaluation | Defer as candidate work until commit or reject clearly in load phase; pure reads may run against explicit candidate context |
| Persistent `smelt.state` | Flush the live generation before loading; let the candidate read the committed snapshot; a flush failure aborts before candidate creation |
| CLI flag declarations from `early.lua` | Launch-only after argument parsing; reload validates declarations but cannot mutate the parsed CLI surface |
| MCP/LSP process state, auto-reload watcher, permissions handle, engine | Never mutate during Lua evaluation; update only from the post-commit effect plan |

A fresh `LuaRuntime` is the preferred candidate boundary because old handles remain valid if candidate loading fails. If a fresh runtime cannot load bundled Lua without mutating live TUI state, add a generation-scoped registration sink for those APIs. Do not fall back to snapshotting Rust maps around an in-place Lua reload: `package.loaded`, globals, coroutines, and handles make that rollback incomplete.

Inject the launch-parsed custom CLI flag values into every candidate by name. If `early.lua` adds, removes, or changes a flag specification after launch, commit the generation using the already parsed values/defaults and show one restart-required warning; never attempt to reparse process arguments during reload.

Commit order for a successful Lua reload:

1. Candidate load and desired-state snapshot succeed.
2. Pure runtime resolution and validation succeed.
3. Enter the existing safe point.
4. Swap the live `LuaGeneration` and authoritative resolved runtime.
5. Retire old generation resources and cancel old generation tasks/watchers.
6. Apply synchronous effects and submit revisions to async controllers.
7. Refresh layout and publish signals.
8. Refresh agent prompt inputs only for reasons that request it.
9. Run the new generation's `ready` hooks.

If candidate loading or pure resolution fails, steps 3 through 9 do not run. Show one deduplicated sticky error and retain the old generation. A later successful reload clears that error. External controller failures after commit do not roll the Lua generation back; they enter a visible degraded state and retry/converge to the latest desired revision.

## Core state and value types

The exact module split can follow dependency constraints, but the ownership must remain non-overlapping.

### `LuaDesiredState`

Collect one typed, read-only snapshot from the candidate after all Lua chunks finish:

```rust
struct LuaDesiredState {
    // Already owns providers, settings, defaults, remember, and MCP.
    config: smelt_core::config::Config,
    modes: ModeDeclarations,
    permissions: PermissionDeclarations,
    lsp: smelt_core::lsp::LspConfig,
    default_shell: Option<smelt_core::lua::DefaultShell>,
}

struct ModeDeclarations {
    cycle: Vec<protocol::AgentMode>,
    behaviors: HashMap<String, smelt_core::permissions::ModeBehavior>,
}

struct PermissionDeclarations {
    rules: smelt_core::permissions::rules::RawPerms,
    tool_defaults: smelt_core::permissions::rules::ToolDefaults,
}
```

Do not duplicate `settings`, `mcp`, `defaults`, or `remember` beside `Config`; `Config` already owns them. Replace destructive `take_permission_rules` with snapshot/clone semantics. Prefer one `LuaRuntime::snapshot_desired_state() -> Result<LuaDesiredState, Error>` call so validation and lock ordering are centralized rather than collecting each field under separate locks.

`smelt.settings.notifications` and `smelt.settings.transcript` are currently raw Lua tables, not members of `ResolvedSettings`. Keep them generation-local unless cross-generation runtime consumers need them. If they need reconciliation, give them typed snapshots instead of silently omitting them.

### `StartupOverrides`

Store actual immutable precedence values parsed at launch:

```rust
struct StartupOverrides {
    model: Option<String>,
    api_base: Option<String>,
    api_key_env: Option<String>,
    provider_type: Option<String>,
    mode: Option<protocol::AgentMode>,
    mode_cycle: Option<Vec<protocol::AgentMode>>,
    reasoning_effort: Option<protocol::ReasoningEffort>,
    reasoning_cycle: Option<Vec<protocol::ReasoningEffort>>,
    model_config: protocol::ModelConfigOverrides,
    settings: HashMap<String, smelt_core::config::SettingValue>,
    request_audit_env: Option<protocol::RequestAuditMode>,
}
```

This replaces `cli_model_override`, `cli_api_base_override`, `cli_api_key_env_override`, and `cli_mode_cycle_override`. Extend `ModelConfigOverrides` with `tool_calling` so `--no-tool-calling` follows the same patch path as temperature/top-p/top-k instead of adding another boolean to runtime resolution. The object also retains typed `--set`, provider type, reasoning, and environment-enforced request-audit values. Resolution reapplies these values on every reload. Mutable current state is never used to reconstruct launch precedence.

### `ManagedModels`

Use one map rather than one field per provider:

```rust
struct ManagedModels {
    providers: HashMap<engine::auth::AuthProvider, ManagedProviderModels>,
}

struct ManagedProviderModels {
    models: Vec<protocol::ModelMetadata>,
    status: ManagedRefreshStatus,
    request_id: u64,
    auth_revision: u64,
    desired_revision: u64,
}

enum ManagedRefreshStatus {
    Idle,
    Refreshing,
    Fresh,
    Cached { warning: Option<String> },
    Failed { message: String },
    Unauthenticated,
}
```

For this refactor, declare Codex, Copilot, and Kimi Code singleton managed-provider kinds. Reject duplicate configured instances of one managed auth kind with a clear validation error. This matches the current auth/cache identity and is simpler than pretending multiple instances are supported. If multi-account support is added later, introduce an explicit non-secret account/provider identity key then.

Cache files are persistence only. This in-memory object is the live catalog input. Cache envelopes should include schema version, provider kind, fetch time, and a non-secret account fingerprint when the auth provider exposes one; ignore incompatible or wrong-account entries. Cache writes occur only for validated fresh results and must be atomic. A failed or unauthenticated refresh must not replace useful models with an empty list.

### Model selection and `ActiveModel`

Keep desired selection separate from the currently usable target:

```rust
struct ModelSelectionState {
    requested_key: Option<String>,
    requested_by: ModelSelectionSource,
    active: Option<ActiveModel>,
}

struct ActiveModel {
    key: String,
    model_name: String,
    api_base: String,
    api_key_env: String,
    provider_type: String,
    config: protocol::ModelConfig,
    availability: ModelAvailability,
}
```

`requested_key` survives an empty catalog, session restore, and an in-flight managed refresh. `ActiveModel.key` is stored directly and is the only session/recent identity. Remove reverse lookup by transport fields. API keys are resolved only when constructing an engine command.

Move `ModelConfig` from `smelt-provider` to `smelt-protocol` and derive `Serialize`, `Deserialize`, and `PartialEq`. Move the small serializable `RequestAuditMode` enum there with `RequestRuntimeConfig` as well. `smelt-provider` and `smelt-engine` already depend on `smelt-protocol`, so protocol cannot refer back to their types without a dependency cycle. Provider and engine code should consume protocol values; do not create duplicate wire config types. Use deliberate `PartialEq` semantics for floating-point fields and reject non-finite Lua/CLI values during validation.

### Authoritative `RuntimeState`

Replace `AppConfig` with one current state rather than mirroring it:

```rust
struct RuntimeState {
    revision: u64,
    settings: smelt_core::config::ResolvedSettings,
    defaults: smelt_core::config::DefaultsConfig,
    remember: smelt_core::config::RememberConfig,
    mode_cycle: Vec<protocol::AgentMode>,
    active_mode: protocol::AgentMode,
    reasoning_cycle: Vec<protocol::ReasoningEffort>,
    reasoning_effort: protocol::ReasoningEffort,
    providers: Vec<smelt_core::config::ProviderConfig>,
    available_models: Vec<smelt_core::config::ResolvedModel>,
    model_selection: ModelSelectionState,
    permissions: PermissionsHandle,
    mcp: HashMap<String, smelt_core::mcp::McpServerConfig>,
    lsp: smelt_core::lsp::LspConfig,
}
```

`Core` owns this state. Remove the old scattered model fields and CLI booleans in the same migration. A short-lived pure `ResolvedRuntime` may carry the candidate values before apply, but it is not stored alongside `RuntimeState` after commit.

Migrate all important no-model consumers explicitly: core signal seeding, context identity, turn preparation, session keys and restore, recent state, Lua model/config APIs, context-window fetches, status/prompt rendering, and headless startup. Do not add an empty-string compatibility sentinel.

### Permission policy and approval ownership

Make the mixed snapshot/live behavior explicit:

```rust
struct PermissionsHandle {
    policy: Arc<RwLock<Arc<PermissionPolicy>>>,
    approvals: Arc<RwLock<RuntimeApprovals>>,
}

struct PermissionSnapshot {
    policy: Arc<PermissionPolicy>,
    approvals: Arc<RwLock<RuntimeApprovals>>,
}
```

Static compiled rules, mode behavior, workspace restriction, roots, and the Lua path resolver belong to `PermissionPolicy`. Session and workspace approvals live in the stable shared `RuntimeApprovals` store. A turn snapshots the current policy at start but intentionally observes later approvals through the shared store. Long-lived services ask the handle for a fresh snapshot per operation.

If splitting the existing `Permissions` type is too disruptive in the first change, preserve these exact semantics behind the handle and make the split before removing old permission paths. Do not imply that cloning `Permissions` makes approvals immutable, because it currently shares their `Arc<RwLock<_>>`.

### `RuntimePlan` and outcome

```rust
struct RuntimePlan {
    next: ResolvedRuntime,
    effects: RuntimeEffects,
    outcome: ReconcileOutcome,
}
```

`RuntimeEffects` is a concrete struct or enum list for TUI invalidation, signals, engine messages, context-window fetches, permission publication, and desired revisions for MCP/LSP/watcher/model-refresh controllers. `ReconcileOutcome` records changed components, warnings/errors, revision, and pending async convergence without secrets. It owns sticky warning keys so success can clear stale warnings.

## Reconciliation pipeline

Place pure resolution in core/app-level code that startup, TUI, and headless can share. Keep effect application in TUI. Tentative entry points:

```rust
fn resolve_runtime(
    inputs: &RuntimeInputs,
    previous: Option<&RuntimeState>,
) -> Result<ResolvedRuntime, ResolveError>;

fn diff_runtime(current: Option<&RuntimeState>, next: ResolvedRuntime) -> RuntimePlan;

impl TuiApp {
    fn apply_runtime(&mut self, plan: RuntimePlan, request: ReconcileRequest)
        -> ReconcileOutcome;
}
```

`RuntimeInputs` borrows one coherent snapshot of committed Lua declarations, startup overrides, cwd/project context, auth state, managed models, recent/session selection intent, and approval state. It does not read mutable globals piecemeal during resolution.

### Request coalescing

One reason enum is too lossy when events arrive together. Use a request with accumulated causes and dirty components:

```rust
struct ReconcileRequest {
    causes: ReconcileCauses,
    dirty: RuntimeDirty,
    lua_reload: Option<LuaReloadRequest>,
}
```

Coalescing rules:

- Merge dirty component bits and keep the newest input revision.
- Manual reload wins over automatic reload for `refresh_agent_inputs` behavior.
- A newer cwd/project/trust target replaces an older pending target.
- Destructive Lua work waits for the existing safe point. Managed model, auth, approval, and controller-status events can update overlays while a turn is active.
- At the safe point, load only the latest pending Lua target. If another reload request arrives during candidate load, finish/discard that candidate and immediately resolve the newer request before commit.
- Equal inputs become a no-op plan.

### Pure resolution order

1. Validate the committed/candidate `LuaDesiredState` and managed-provider singleton invariant.
2. Resolve settings from schema defaults, Lua values, typed `--set` values, and immutable environment policy.
3. Resolve static providers and inject only currently authenticated managed providers.
4. Merge static and managed model metadata deterministically.
5. Resolve pending/current model selection and `ActiveModel` without resolving API-key values.
6. Resolve mode and reasoning cycles and selections, applying immutable CLI precedence.
7. Compile permission policy from declarations, settings, cwd/project roots, and mode behaviors while retaining the stable approval store.
8. Resolve MCP, LSP, watcher manifest, and other controller desired values.
9. Return the complete candidate state plus warnings. Do not mutate recent/session values while resolving.

### Apply order

1. Receive a precomputed diff against the authoritative current state.
2. If this is a Lua reload, swap the validated generation.
3. For a non-no-op plan, increment the runtime revision once and install changed resolved values as one app-loop operation.
4. Replace current permission policy through `PermissionsHandle`.
5. Apply synchronous TUI effects and invalidate only affected render/cache generations.
6. Publish coherent signals after state installation, not one field at a time.
7. Persist recent/session selection only if the user-visible selection actually changed.
8. Send complete turn/model commands only for explicit active-turn actions; background metadata updates change future-turn state only.
9. Submit `(revision, desired)` to MCP, LSP, auto-reload, model-refresh, and context-window controllers. Submission must be non-blocking.
10. Render warnings/status. For a committed Lua generation, refresh layout and then run `ready` hooks.

Every command-drain site in the engine must either route through one shared command handler or have an exhaustive test. Do not add a runtime command in only the idle path while missing in-request, concurrent-tool, or tool-result wait paths.

### Effect failure policy

Synchronous validation failure aborts before commit. A synchronous TUI apply failure should be treated as an invariant violation and tested, not silently ignored. Asynchronous service failure does not roll back committed value state; the relevant controller retains latest desired state, reports a keyed degraded status, and retries according to its policy. A later success clears the status. `ready` means the desired MCP/LSP revisions were submitted, not that network handshakes completed.

## Settings reconciliation

`smelt.settings.__newindex` must always update the candidate/live Lua desired store first. In running state it queues one non-reentrant reconcile request. `__index` should consult that store so the same Lua callback has read-your-write behavior even though host effects apply after the callback returns. During candidate load it records declarations only; the candidate is reconciled once after the chunk succeeds. `set_settings` becomes the one synchronous TUI effect applier used by `apply_runtime`, not an alternate source of desired values.

Every setting declaration must carry or be covered by an exhaustively tested reload classification. Adding a setting without a classification should fail a test. Initial audit:

| Settings | Reload behavior |
| --- | --- |
| `vim`, `system_clipboard` | Immediate TUI input behavior update |
| `show_tps`, `show_tokens`, `show_cost`, `show_slug`, `show_tips` | Live render read plus relevant signal/render invalidation |
| `show_prediction` | Live prompt behavior; clear an existing prediction when disabled |
| `file_icons`, `file_icon_colors` | Invalidate inline options and transcript renderer caches |
| `terminal_title` | Immediately update or restore the terminal title and publish its signal |
| `restrict_to_workspace`, `worktree_root` | Re-resolve permission policy and workspace approval roots; `worktree_root` also affects future managed worktree creation |
| `auto_reload` | Submit desired state to `AutoReloadController` |
| `redact_secrets`, `cache_ttl_long`, `request_audit` | Snapshot into future turns and `EngineAsk` requests; environment request-audit policy wins |
| `auto_compact`, `compact_threshold`, `compact_keep_recent_groups` | Live read by the bundled compact policy on its next decision |
| `auto_continue` | Live read by idle continuation policy; wake/re-evaluate the idle controller |
| `web_search_provider`, `brave_search_api_key_env` | Live read at the next web-search invocation; never resolve or log the key during reconciliation |
| `autoupgrade`, `autoupgrade_channel`, `autoupgrade_interval` | Reconfigure/wake the upgrade controller without leaving the previous timer active |

Do not add `UiCommand::SetRuntimeConfig` merely to mutate `EngineConfig` while a `Turn` holds `&EngineConfig`. That creates difficult command-drain semantics and another mutable global. Prefer a serializable `RequestRuntimeConfig` carried by `StartTurnPayload` and `EngineAsk`:

```rust
struct RequestRuntimeConfig {
    redact_secrets: bool,
    cache_ttl_long: bool,
    request_audit: RequestAuditMode,
}
```

A running turn keeps this snapshot stable; a new turn or one-shot ask gets current resolved values. This makes reload semantics deterministic and removes the need to update every engine wait loop for settings commands. If product requirements later demand a setting change during an active turn, add a dedicated live policy handle with a precise read boundary rather than mutating borrowed `EngineConfig`.

Settings with Lua table values, currently `notifications` and `transcript`, need an explicit ownership test: they either remain entirely inside the committed Lua generation or become typed fields with a documented effect. They must not be accidentally copied as stale handles into `RuntimeState`.

## Mode and reasoning reconciliation

Use the same selection pattern as models rather than assigning defaults on every reload:

- Launch priority is immutable CLI value, remembered value when enabled, Lua default, then the built-in fallback.
- CLI mode/reasoning cycles remain immutable inputs and are reapplied after every Lua generation change.
- Otherwise, reload replaces declared cycles but retains the current user selection when it is still valid. A changed `defaults` or `remember` declaration does not retroactively switch it.
- If a current mode is removed, choose registered `normal` when available, otherwise the first declared mode, and report the fallback once. Validate/deduplicate empty and duplicate cycles before commit.
- Reasoning selection remains the user's requested effort. Provider capability computes effective effort from the turn's `ModelTarget`; a metadata refresh does not silently rewrite the user preference.
- Explicit user mode/reasoning changes during an active turn apply at the next provider-request boundary. Background Lua/default/metadata changes affect future turns only.
- Permission evaluation for an active turn always uses the mode carried by that turn/event, even when the UI's current mode has changed.

Persist remembered mode/reasoning only for explicit or real fallback selection changes, not no-op reconciliation.

## Permissions reconciliation

Extract one resolver used by startup, reload, settings changes, cwd/project changes, and workspace approval sync:

```rust
fn resolve_permission_policy(input: PermissionResolveInput)
    -> Result<PermissionPolicy, PermissionResolveError>;
```

Inputs are raw Lua rules, tool defaults, mode behaviors, resolved settings, explicit project/cwd context, workspace roots, and the candidate generation's Lua path-resolver handle. Approval contents are not copied through the pure policy resolver; they remain in the stable approval store.

Responsibilities:

- Compile and validate raw rules.
- Apply tool and mode defaults.
- Apply `restrict_to_workspace`.
- Recompute allowed roots from cwd and `worktree_root`.
- Atomically replace workspace-derived approvals for the current allowed-root set, removing grants from roots that are no longer active, without discarding session approvals or session path grants.
- Bind the Lua path resolver from the committed generation.
- Publish policy only after the generation commits.

Lua policy, mode, setting, and cwd/project policy changes affect future turns. Active turns retain their static policy snapshot while observing newly granted session/workspace approvals. Because cwd changes process/tool behavior and project Lua together, queue the entire cwd transaction until the Lua safe point instead of replacing policy under a running turn.

Fix Lua tool permission evaluation to use both the active turn's `PermissionSnapshot` and the `mode` carried by `EngineEvent::ToolEvaluationRequest`. The current handler ignores the event mode and can evaluate an old turn under a newly selected UI mode.

Update `McpDispatcher` to hold `PermissionsHandle`, take one fresh `PermissionSnapshot` at the start of each MCP operation, and use it consistently for that operation. Do not hold the handle's lock across async work or user approval UI.

## Convergent service controllers

### MCP

Do not spawn one independent `manager.reconcile(desired)` task per app reconcile. Two such tasks can acquire the map lock out of order and restore an older server map. Make latest-desired convergence mandatory:

```rust
McpController::set_desired(revision, desired);
```

A single worker owns desired revision and map mutation. Submission replaces the pending desired value and wakes the worker. The worker applies removals/config replacements before launching connectors, cancels obsolete connector tokens, and remains able to consume a newer desired value instead of awaiting every handshake serially. Each connector carries server identity/config revision and verifies it before publishing tools or status. Removed servers stop being dispatchable immediately, even if an uncancellable old handshake later completes.

Use `RuntimeState.mcp` as desired state, keep `McpManager` stable behind `Arc` for engine tool lookup, and expose observed revision/status separately. Pair dispatch with `PermissionsHandle` as described above.

### LSP

`smelt.lsp.configure` currently mutates the long-lived `LspManager` during Lua evaluation. Change it to record `LspConfig` in the candidate. Absence in the new generation means an empty configuration, so removing the call stops obsolete servers. Submit committed `RuntimeState.lsp` to a latest-desired LSP controller with the same revision rules as MCP. `smelt.lsp` calls continue to use the stable manager, but candidate load never starts or stops a server.

### Auto-reload watcher

Move watcher handle, receiver, setup request id, accepted content snapshot, and desired paths into an `AutoReloadController` owned by `TuiApp`.

- `auto_reload = false` invalidates pending setup and drops the active watcher.
- Enabling it, committing a new `LuaLoadManifest`, changing cwd/project trust, or changing config/plugin roots submits a new desired revision.
- A stale `spawn_blocking` setup result cannot install a watcher after disable or after newer paths are desired.
- The controller watches the last committed manifest plus loader roots needed to detect additions. A failed candidate does not replace the committed manifest, but files/roots encountered before its failure may be added as retry-only dependencies so fixing them triggers another candidate load; they never activate candidate callbacks.
- Content snapshots, not raw notify events, remain the reload source of truth. Suppress self-generated writes and coalesce editor save bursts without creating reload loops.

### Controller status

MCP, LSP, watcher, managed model refresh, and context-window fetches report `{ desired_revision, observed_revision, status }` back through `AppEvent`. The app ignores events whose identity/revision no longer matches. A context-window request identity includes the active key plus transport/config fingerprint, not only model/base/provider strings, so metadata-only changes cannot accept an older result. Status events may render warnings but must not recursively submit unchanged desired state.

## Model catalog and selection

Move model resolution out of `src/startup.rs` into shared pure code used by startup, headless, and runtime reconciliation.

### Deterministic catalog merge

1. Start from validated Lua provider declarations in declaration order.
2. Inject one OAuth transport for each configured and authenticated managed provider kind.
3. Retain every explicit static model and alias. Dynamic refresh must never delete user aliases.
4. Merge dynamic metadata into matching models. Explicit user config wins; provider metadata fills unset fields.
5. Add remaining provider-discovered canonical models in deterministic provider/name order after explicit entries.
6. Inherit provider transport values, including `api_key_env`; do not synthesize an empty key-env value for dynamic models.
7. Preserve display names and all supported metadata in `ResolvedModel`. Extend `ModelMetadata` where needed, including max output tokens, instead of dropping data during conversion.
8. Treat duplicate final keys as validation errors with provider/source context. Define and test the existing case-sensitivity/reference rules rather than relying on `HashMap` insertion order.

Derive semantic `PartialEq` for model transport/config so no-op refreshes do not trigger downstream work.

### Selection policy

Selection intent and usable transport are resolved separately:

- On launch or while no model is active, priority is immutable CLI selection, pending session selection, remembered model when enabled, `smelt.defaults.model`, then first available model.
- Apply CLI base/key-env/provider-type/model-config values after catalog selection. Preserve the existing explicit direct-target case where `--model` is absent from the catalog but `--api-base` supplies a usable transport; give that target a stable synthetic key instead of inserting it into the Lua catalog.
- A session key that is absent only because a managed cache is empty remains pending and is retried after refresh. Do not silently discard it.
- Reloading `defaults` or `remember` affects future fallback/remember behavior, not an already active selection.
- An explicit user selection updates `requested_key` and resolves immediately or returns a clear error.
- If the active key still exists, refresh the app's metadata for future turns without mutating a running turn.
- If the active key disappears from a committed catalog, retain its complete transport as `StaleCatalog` and show a keyed warning. Do not silently switch providers.
- If auth is revoked or an API key becomes unavailable, retain the key for honest session/status reporting but mark it `Unavailable`; do not start a request with it.
- Persist recent/session model identity only on a real selection change. Never overwrite useful state with no-model during asynchronous startup.

Use the availability enum from `ActiveModel` so catalog staleness and unusable credentials are not collapsed into one boolean:

```rust
enum ModelAvailability {
    Available,
    StaleCatalog,
    Unavailable { reason: ModelUnavailableReason },
}
```

Starting a turn with no model or an unavailable model fails in TUI before engine dispatch with a specific status. `StaleCatalog` may start only when the retained target still has valid credentials and transport.

### Lua model/config API

Choose one nil convention and document it in generated Lua docs:

- `smelt.model.current()` returns the active **key**, or `nil`.
- `smelt.model.status()` always returns a table with current/requested key, availability/reason, per-provider refresh status, and a sanitized last error.
- Active-value helpers such as capabilities, pricing, token limits, modalities, transport, and `smelt.config.*` return `nil` when no active target exists.
- `smelt.model.transport()` exposes key environment variable name, never its resolved value.
- `smelt.model.set(name)` errors for an unresolved name. Re-selecting the same usable target is an idempotent no-op.

Update Lua API stubs/reference docs in the same change. Audit plugins for assumptions that `current()` is always a string or that it returns the bare transport model name.

## Canonical engine model target

Define one serializable target in `smelt-protocol` and use it on every TUI-to-engine model path:

```rust
struct ModelTarget {
    model: String,
    api_base: String,
    api_key: String,
    provider_type: String,
    config: protocol::ModelConfig,
}
```

`ActiveModel::to_target()` resolves `api_key_env` at dispatch and returns an error without logging the secret. Sampling overrides from custom commands are merged into a cloned target config before dispatch.

Use `ModelTarget` for:

- `StartTurnPayload`
- explicit active-turn model changes
- `EngineAsk` / the current `AskModel` path
- custom command provider/model overrides
- headless turn startup

Prefer removing engine-global interactive model synchronization:

- `StartTurnPayload.model_target` is authoritative for a new turn.
- A `Turn` owns its target, provider, capabilities, and pricing context.
- Rename `UiCommand::SetModel` to `SetTurnModel { target }` and send it only for an explicit user model switch while a turn is active. Apply it at the next provider-request boundary.
- Background refresh or Lua metadata changes update `RuntimeState` for future turns and never send `SetTurnModel`.
- When no turn is active, selecting a model requires no engine command; the next `StartTurn` carries the full target.
- Keep startup `EngineConfig.api` only where headless/bootstrap still needs it, then remove interactive reliance on it rather than maintaining two active targets.

`EngineAsk` must always carry a complete target config. When Lua omits a model override, the TUI resolves the current `ActiveModel` and sends its full target; the engine does not fill fields from global config. The current ask path resolves transport fields but substitutes the globally active `model_config`; remove that fallback. Likewise, custom commands must not combine an override provider/model with active provider type or active capabilities.

Usage accounting and pricing must read the target owned by the turn/request. It must not read `self.config.api` after a per-turn or mid-turn target change. Rebuilding a provider and updating pricing context happen together from the same `ModelTarget`.

Centralize `SetTurnModel` command handling so idle, active turn, in-flight request, concurrent-tool, and tool-result wait loops cannot drift. Define cancellation semantics: an in-flight HTTP request keeps its target; the new target applies to the next request. Add serialization round-trip tests for every payload containing `ModelTarget` and `RequestRuntimeConfig`.

## Background managed model refresh

The current auth facade returns only `Vec<ModelMetadata>`, while providers variously return cached data as success, erase errors into empty lists, or persist empty lists. That API cannot support truthful refresh status. Replace it with a typed outcome:

```rust
struct RefreshToken {
    provider: engine::auth::AuthProvider,
    request_id: u64,
    auth_revision: u64,
    desired_revision: u64,
}

enum ManagedModelsRefreshOutcome {
    Fresh(Vec<protocol::ModelMetadata>),
    CachedFallback {
        models: Vec<protocol::ModelMetadata>,
        warning: String,
    },
    Unauthenticated,
    Failed(String),
}

AppEvent::ManagedModelsRefreshCompleted {
    token: RefreshToken,
    outcome: ManagedModelsRefreshOutcome,
}
```

Provider adapters must distinguish a fresh authoritative empty list from network/auth/parse failure. Only `Fresh` updates cache files. `CachedFallback` preserves models but surfaces degraded freshness. `Failed` preserves the previous in-memory and on-disk list. Logout changes status to unauthenticated and makes active managed targets unavailable without erasing session identity.

A completion applies only when all token dimensions still match and the provider is still desired and authenticated. Provider kind alone is insufficient because logout/login, account changes, config reload, and a second refresh can occur while a request is in flight. Increment `auth_revision` for login, logout, or detected external credential identity change; never put account tokens or identities in logs.

Startup behavior:

- Load validated cache files into `ManagedModels` before the first resolve.
- Render the first interactive frame from cached/static state without network waits.
- Submit one refresh per desired authenticated provider after startup.
- Update the in-memory outcome in the app event loop, then run pure resolution.
- Coalesce duplicate refresh requests and use bounded retry/backoff for transient failures.

Interactive TUI remains non-blocking. Headless should initially fail clearly when no model can be resolved from static/cache state. A bounded headless refresh can be added later as an explicit CLI policy, not an implicit timing-dependent startup behavior.

## Cwd, project config, and trust transitions

Cwd changes currently rebuild permission roots but do not replace the loaded project Lua config. The new architecture treats project context as part of the Lua generation:

1. `smelt.session.switch_cwd`, managed worktree entry, and session restore submit a cwd transaction instead of mutating process cwd inline from a Lua callback.
2. Validate the target directory and determine project/trust state.
3. Load a candidate generation against the target cwd using explicit paths rather than temporarily changing process-global cwd during candidate evaluation. Config-time cwd/project APIs read the candidate context, not live app globals.
4. Resolve candidate runtime and permission policy for the target project.
5. At the Lua safe point, when no turn or callback can observe an intermediate state, commit process cwd, project context, Lua generation, permission policy, engine cwd, session metadata, prompt inputs, and watcher desired paths together.
6. If candidate config fails, reject the cwd transition and keep the previous cwd/generation. Report the config error and allow retry after the file is fixed.

The Lua cwd/worktree API must complete asynchronously after commit or return a clearly pending operation; it must not return the old cwd as if the switch succeeded. A request made during an active turn waits until that turn ends. If asynchronous API migration is staged, reject busy-state cwd changes until it lands rather than updating a running turn's cwd and permission policy in place.

A trust denial is not a load failure: commit the target cwd with global config only and record that project config was skipped. A later trust-state change queues a new candidate reload. `worktree_root` changes workspace/worktree policy but does not itself pretend that cwd changed. Worktree creation can succeed even if entering it is later rejected by target project config; report both facts clearly.

This transaction prevents old project commands/tools/hooks from silently remaining active in a new project and prevents new permission roots from being combined with old project Lua.

## Lua handles, turn snapshots, and lifecycle

Lua handles and generation-owned resources are never copied into declarative `RuntimeState`. A successful generation swap retires the old command/keymap/tool/hook/renderer/callback/task/watcher registries together. Non-destructive managed-model, settings, approval, auth, and controller-status reconciliation does not touch them.

Turn snapshot contract:

| Data | Active-turn behavior |
| --- | --- |
| Lua tool definitions and tool hooks | Snapshot during turn preparation |
| Static permission policy and cwd/project context | Snapshot at turn start; cwd transactions wait for the turn to finish |
| Session/workspace approvals | Shared and live |
| Mode used for tool evaluation | Event/turn mode, not current UI mode |
| Model target, mode/reasoning, and request runtime config | Snapshot at turn start |
| Explicit user model/mode/reasoning switch | Apply complete value at next provider request boundary |
| Background model metadata, settings reload, provider/mode catalog | Future turns only |
| MCP/LSP server/tool state | Long-lived manager observes latest committed desired revision |

Provider middleware and engine hooks follow the same generation safety rule: an invocation snapshots the hook list it will call. Candidate work cannot replace live hooks. Generation commit waits for the existing Lua safe point so no in-flight Lua callback is stranded.

`smelt.lifecycle.on("ready", ...)` runs only for a successfully committed generation, after coherent signals and layout are current. Hooks observe updated value state and desired service revisions, but MCP/LSP network convergence may still be pending. Failed candidates do not drain old or candidate `ready` hooks.

Manual `/reload` refreshes AGENTS.md, skills, and explicit system-prompt input after commit. Automatic config reload does not refresh those prompt inputs. A committed cwd/project transition refreshes project-scoped prompt inputs because their lookup root changed.

## Migration strategy

Keep changes reviewable, but do not leave two live sources of truth at a phase boundary. Each phase removes the path it replaces.

### Phase 0: Characterization and surface audit

- Add E2E harness reproductions for failed reload destruction, stale provider/model state, stale MCP permissions, and background model results not reaching the UI.
- Inventory every `LuaShared` field and every Lua API callable during load. Classify it as declaration, generation-owned handle/resource, launch-only, persistent, or post-commit external effect.
- Inventory every active-model read and every engine command-drain site.
- Add test-only deterministic barriers/fakes for model refresh, MCP/LSP connection, and watcher setup. Race tests must not depend on sleeps.

Exit gate: the reload ownership ledger is complete and the current failures are reproducible from user-visible behavior.

### Phase 1: Canonical engine request values

- Move `ModelConfig` into `smelt-protocol` and add `ModelTarget` plus `RequestRuntimeConfig`.
- Put complete targets on `StartTurnPayload`, `EngineAsk`, custom-command overrides, and headless startup.
- Make each engine turn/request own target, provider, request config, and pricing context.
- Replace global `SetModel` synchronization with explicit `SetTurnModel` semantics.
- Centralize command handling and remove interactive fallbacks to unrelated global model config.

Exit gate: all model request paths serialize the same complete target; pricing/capabilities use that target; no provider/config mixing remains.

### Phase 2: One resolved runtime state

- Add typed `StartupOverrides`, `ModelSelectionState`, and pure startup/runtime resolver inputs.
- Replace `AppConfig` with authoritative `RuntimeState`; remove CLI booleans and scattered model transport fields.
- Move startup/headless/TUI onto the shared provider/model/mode/settings resolution.
- Implement explicit no-model and pending-selection behavior across session, status, signals, and Lua APIs.
- Make settings writes update desired state first and add the per-setting effect classification test.

Exit gate: startup and a no-op runtime resolve produce equivalent state; no legacy mirrored fields remain.

### Phase 3: Transactional Lua generations

- Add `LuaGeneration`, `LuaLoadManifest`, and one desired-state snapshot API.
- Make settings, MCP, and LSP declaration-only during candidate load. Until Phase 6 hardens controllers, apply committed MCP/LSP desired state through one post-commit path so current behavior is not lost.
- Scope registries, tasks, filesystem watchers, callbacks, and TUI Lua resources to a candidate/live generation.
- Load and validate a fresh candidate before retiring the old generation.
- Correct failed-load and `ready`-hook behavior. Make the candidate loader accept an explicit target cwd/project/trust context, but do not switch `/cd` to it until permission commit is included in Phase 4.

Exit gate: syntax/runtime failure at every load phase leaves all old observable behavior intact and a subsequent fixed reload succeeds without restart.

### Phase 4: Permissions and settings effects

- Split or formalize permission policy versus shared approval ownership.
- Introduce `PermissionsHandle`; update core, active turns, Lua evaluation, and MCP dispatch.
- Use the shared policy resolver for startup, reload, setting change, cwd transaction, and approval sync.
- Commit cwd, project/trust generation, permission roots, engine cwd, session metadata, and prompt inputs as one safe-point transaction; migrate/reject synchronous busy-state cwd APIs.
- Implement all synchronous setting effects and future-turn request settings.
- Remove scattered permission reconstruction and ad hoc `set_settings` mutations.

Exit gate: policy/approval/active-turn semantics match the snapshot table and all settings have tested reload behavior.

### Phase 5: Managed model live state

- Introduce `ManagedModels` and typed refresh outcomes.
- Normalize provider refresh/cache error behavior and atomic cache writes.
- Submit refresh results to the app loop with request/auth/desired revisions.
- Re-resolve the catalog, pending selection, capabilities, and context-window request in memory.
- Handle login, logout, external credential changes, empty caches, stale catalog entries, and no-op metadata.

Exit gate: a running TUI converges without restart and no stale result can regress auth/catalog/selection state.

### Phase 6: Convergent controllers

- Replace independently spawned MCP reconciliation with one latest-desired controller.
- Move LSP configuration to desired state and add its controller.
- Move watcher ownership/setup into `AutoReloadController` and drive it from committed manifests.
- Put context-window fetches on the same identity/revision discipline.
- Add structured controller status and warning clearing.

Exit gate: deterministic out-of-order tests pass for every controller and repeated equal desired values cause no restarts.

### Phase 7: Cleanup, docs, and hardening

- Delete old reload clears/reconcile helpers, startup model injection paths, reverse model-key lookup, stale CLI flags, and temporary projections.
- Regenerate Lua API docs/stubs and update user-facing reload/model semantics.
- Extend repeated-reload fuzzing and handle/resource leak assertions to failed candidates.
- Run format, clippy, workspace tests, storybook snapshots where UI status changed, and coverage gate.

## Test plan

Start with the closest E2E behavior, then cover pure resolution and races below it.

### Transaction and TUI harness tests

- Start with a working config that registers a command, keymap/leader, tool, hook, renderer, provider, MCP server, LSP server, default shell, notification/transcript config, and watcher. Introduce a syntax error and separately a runtime error in early, autoload, user, project, and plugin load phases. Every old behavior remains active, no candidate `ready` hook runs, and fixing the file commits exactly once.
- A failed candidate does not clear prompt/UI callbacks, anonymous/named resource ownership, pending Lua tasks, or current providers/settings/permissions, and does not launch candidate process/network/filesystem mutations.
- Reload adding/removing providers updates `smelt.model.list()` but does not silently change an active selection.
- Reload removing MCP/LSP declarations converges to an empty desired set; failed reload preserves the old set.
- Settings declarations are captured during load without reentrant partial application. Runtime assignment uses the same store/effect path.
- Every setting in the schema exercises its classified effect, including terminal title restoration, prediction clearing, icon cache invalidation, auto-reload disable, and upgrade timer replacement.
- Manual reload refreshes prompt inputs; automatic reload does not; cwd/project transition follows its explicit policy.
- No-op reload/reconcile emits no model command, controller restart, session write, or extra warning.
- Sticky failure status clears after the corresponding successful candidate/controller revision.

### Resolver and state tests

- Table-test precedence across CLI values, `--set`, environment request-audit policy, remembered state, defaults, session selection, and first model.
- Reapplying CLI transport, mode, reasoning, sampling, tool-calling, and setting overrides after Lua reload yields identical effective values.
- Session restore with an empty managed cache retains pending key and selects it when it appears.
- No-model, stale-catalog, and unauthenticated states never use empty strings and never dispatch unusable targets.
- Static aliases survive managed refresh; explicit metadata wins; dynamic fields fill only unset values; output order and collision errors are deterministic.
- Dynamic models inherit provider key-env and preserve display/capability/token metadata.
- Duplicate managed provider kinds are rejected clearly.
- Permission resolver updates roots/policy, retains shared session approvals, loads workspace approvals for all current roots, and removes workspace grants from departed roots.
- Runtime diff detects real `ModelConfig` changes, ignores equal refresh results, and emits only the necessary effects.

### Active-turn and engine tests

- `StartTurn`, `EngineAsk`, custom command overrides, explicit turn switch, and headless startup use complete `ModelTarget` and request config payloads.
- Custom provider/model overrides never inherit active provider type or active model config accidentally.
- Usage pricing, max tokens, modalities, reasoning, and tool-calling read the target owned by the request.
- Background catalog/config changes leave a running turn target unchanged; the next turn uses new metadata.
- Explicit user model/mode/reasoning switches during an active turn leave an in-flight request alone and affect the next provider request.
- Future-turn settings do not mutate an active turn; the next turn and next `EngineAsk` use current values.
- Changing UI mode during a turn does not change that turn's Lua tool evaluation mode.
- Lua policy reload leaves active static policy stable; a newly granted approval is visible; a cwd request during a turn leaves cwd/policy unchanged until the turn ends, then commits coherently.
- Exercise command arrival in idle, request-in-flight, concurrent-tool, sequential-tool, and tool-result wait states.
- Serialize/deserialize every protocol payload containing `ModelTarget`, `ModelConfig`, and `RequestRuntimeConfig`.

### Async race and failure tests

Use controlled channels/barriers and deliver completions in the wrong order:

- older managed-model success after newer success
- refresh success after logout or provider removal
- old-account completion after login/auth revision change and wrong-account cache rejection
- network failure preserving cached/in-memory models
- authoritative fresh empty result versus failed empty result
- older MCP reconcile/connection after newer desired map
- older LSP configure/start after removal
- watcher setup completing after disable or path change
- context-window result after active target change
- rapid manual/automatic reload coalescing while a candidate is loading

In every case, observed state converges to latest desired revision and stale work cannot clear a newer warning/status or publish tools/models.

### Repetition, leak, and recovery tests

- Repeated successful and failed candidate loads do not grow Lua registry handles, callbacks, tasks, watchers, TUI anonymous resources, MCP/LSP processes, or sticky notifications.
- Extend `fuzz/src/lua_loop.rs` with alternating valid/invalid generations and candidate discard.
- Cache corruption, unwritable cache, missing API key env, unauthenticated provider, MCP/LSP handshake failure, and watcher setup failure remain recoverable and never panic.
- Session persistence records direct model keys and survives no-model startup, stale catalog, logout, and later reauthentication.

## Observability and diagnostics

Expose a sanitized runtime/reload status for tests and user diagnostics:

- committed Lua generation and runtime revision
- last candidate failure phase/path without source contents or secrets
- active/requested model key and availability
- managed provider freshness and request/auth revision
- MCP/LSP/watcher desired versus observed revision
- pending generation-changing reload and whether it is waiting for a safe point

Use stable warning keys per component so repeated failures deduplicate and successful convergence clears them. Structured logs should include revision, component, duration, and result category, never API keys, credential values, full provider payloads, or sensitive Lua setting contents.

## Compatibility and documentation

- TUI/engine wire payloads are internal, so update all producers/consumers atomically rather than carrying both old and new model fields.
- Moving public `smelt_provider::ModelConfig` is a Rust API concern. Prefer updating workspace consumers directly; if published-crate compatibility must be preserved, use a temporary `pub use protocol::ModelConfig` re-export marked and scheduled as compatibility debt rather than duplicating the type.
- `smelt.model.current()` returning an optional key and active helpers returning `nil` is a Lua API behavior change. Update bundled plugins, tests, generated stubs/reference docs, and release notes together.
- Session files continue storing a string model key; no empty-string migration is needed. Preserve unknown/stale keys as selection intent.
- If any temporary compatibility projection is unavoidable, mark it with `COMPAT(<id>)`, add it to `docs/compat.md`, and remove it in Phase 7.

## Decisions made by this plan

1. Use transactional Lua generations plus explicit value resolution, not post-failure patching and not an all-at-once rewrite.
2. Replace `AppConfig`; do not synchronize it with a new runtime object.
3. Move `ModelConfig` into protocol and use one `ModelTarget` on every engine path.
4. Treat request settings as future-turn/request snapshots, not mutable `EngineConfig` commands.
5. Keep managed auth providers singleton by kind until explicit multi-account identity exists.
6. Keep interactive startup non-blocking and make headless fail deterministically when static/cache state cannot resolve a model.
7. Use `nil` for absent active-model Lua values and a status table for detail.
8. Queue cwd/project transitions to the Lua safe point and reject a transition whose target project candidate cannot load, preserving the prior coherent project context.

Implementation may choose `RwLock`, `arc-swap`, or an equivalent small primitive for live handles and may choose the exact candidate registration sink. Those are local implementation choices as long as the invariants and tests above hold.

## Recommended PR sequence

Do not combine phases 1 through 6 into one review. Recommended vertical sequence:

1. Canonical protocol `ModelConfig`/`ModelTarget` and request-owned engine behavior.
2. `StartupOverrides`, pure resolution, and replacement of `AppConfig` with `RuntimeState`.
3. Transactional Lua generation with failed-reload E2E preservation.
4. Permissions/settings reconciliation, cwd/project transaction, and active-turn policy semantics.
5. Managed models and live refresh convergence.
6. MCP/LSP/watcher controllers, cleanup, and hardening.

The first PR should be item 1 only. It fixes existing provider/config/pricing correctness and creates a clean transport boundary without introducing temporary parallel runtime state. Each later PR should include its E2E test and delete the old path it supersedes.
