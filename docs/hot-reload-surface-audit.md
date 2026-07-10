# Hot reload surface audit

This ledger is the Phase 0 ownership baseline for
[`hot-reload-refactor-plan.md`](hot-reload-refactor-plan.md). It records the
current ownership and the required transactional behavior before the runtime is
refactored. Update this file whenever a Lua API, a `LuaShared` field, an active
model consumer, or an engine command receive loop is added or moved.

## Classification vocabulary

| Class | Candidate-load rule |
| --- | --- |
| Declaration | Record candidate desired state only. The live app is updated after a successful commit. |
| Generation | Store the handle or resource in the candidate generation. Activate it at commit and retire it with that generation. |
| Launch-only | Validate on reload, but preserve values fixed during process argument/bootstrap handling. |
| Persistent | Flush the committed generation before candidate load. Candidate reads the committed snapshot and may stage later writes. |
| External effect | Do not execute during candidate evaluation. Defer until commit, or reject the call in the load phase. |
| Pure read | Read explicit candidate context or immutable process data. It may execute while loading. |
| Runtime-only | Reject during candidate load. It operates on an already committed interactive session. |

## `LuaShared` ownership

Source: `crates/core/src/lua/shared.rs`. Every field in `LuaShared` is listed
below. Atomics that allocate ids or invalidate caches belong to the same owner
as the registry they support.

| Fields | Current role | Target owner/class |
| --- | --- | --- |
| `commands`, `command_names`, `next_registry_token` | Command callbacks and worker-safe name projection | Generation |
| `keymaps`, `keymap_leader` | Global key callbacks and leader declaration | Generation. The leader is a declaration inside the generation. |
| `main_layout_composer`, `win_renderers` | TUI layout/render callbacks | Generation |
| `tools` | Lua tool callbacks and permission metadata | Generation, with permission defaults included in the desired-state snapshot |
| `transcript_renderer`, `transcript_renderer_generation`, `transcript_renderer_cache_key` | Transcript renderer callback and cache identity | Generation |
| `transcript_groups`, `transcript_groups_generation`, `transcript_groups_cache_key` | Transcript group callbacks and cache identity | Generation |
| `callbacks`, `ask_callbacks`, `next_id` | General TUI callbacks and engine-ask callbacks | Generation |
| `next_buf_id` | Lua-owned TUI resource id allocator | Session service, while allocated resources carry generation ownership |
| `next_external_id` | Async Lua operation id allocator | Generation |
| `tasks`, `task_inbox`, `json_inbox`, `wakeup_tx` | Lua coroutines and cross-thread completion delivery | Generation, except the wakeup transport may be a session host service; every message must carry generation identity |
| `providers` | `smelt.provider.register` declarations | Declaration |
| `permission_rules` | Static permission declarations | Declaration |
| `mcp_configs` | MCP desired server map | Declaration |
| `lsp` | Live LSP manager currently mutated by Lua | Split: stable session host service plus candidate LSP declaration |
| `settings_overrides` | Lua setting declarations | Declaration. Runtime writes update this same desired store before reconciliation. |
| `defaults` | Default selection declarations | Declaration |
| `remember` | Remember-policy declarations | Declaration |
| `tool_defaults` | Lua tool permission defaults | Declaration derived from generation tool registrations |
| `messages` | Session message inbox | Session service, not reload-owned |
| `disabled_modules` | Early bootstrap module selection | Launch-only |
| `native_module_names` | Baseline `package.loaded` keys | Runtime construction metadata, immutable after a runtime is created |
| `cli_flag_specs` | Early Lua CLI declarations | Launch-only; reload may only validate and warn that restart is required |
| `cli_flag_values` | Parsed custom CLI values | Immutable startup overlay injected into each candidate |
| `hooks` | Tool/provider/engine/lifecycle callback registries | Generation |
| `default_shell` | Default process shell declaration | Declaration |
| `watchers`, `next_watcher_id` | Live `notify` watchers and ids | Generation; candidate watchers stay paused until commit |
| `phase` | Early/init/running API guard | Generation-local load state |

The TUI wrapper `crates/tui/src/lua/mod.rs::LuaShared` adds
`pending_invocations`. It is generation-owned because every queued invocation
contains a Lua handle.

Long-lived state currently reached through the app pointer, rather than
`LuaShared`, also needs an owner:

- `Core`, engine, MCP manager, LSP manager, clipboard, process registry,
  workspace index, session persistence, and wakeup/event transports are
  session services.
- signals, timers, UI callbacks, paint callbacks, named Lua bindings, picker
  callbacks, and pending Lua invocations are generation-owned projections onto
  session-long TUI containers.
- `RuntimeState` is the single authoritative resolved value state. Lua handles
  and services are not stored in it.

## Lua callable inventory

The generated API index at `docs/docs/reference/api/index.md` is the canonical
callable list. At this audit it contains 84 namespaces and 456 functions. The
classification below covers every namespace in that index. Every function
inherits its namespace classification except for the explicit exceptions in
this section. Methods on generated classes inherit the class's creating
function classification.

### Declarations and generation registrations

| Namespace/function | Class | Notes |
| --- | --- | --- |
| `smelt.defaults` | Declaration | Complete defaults object. |
| `smelt.remember` | Declaration | Complete remember policy. |
| `smelt.provider.register` | Declaration | Provider and static model catalog. |
| `smelt.provider.middleware` | Generation | Provider response callbacks. Other `smelt.provider` helpers are pure reads over candidate declarations. |
| `smelt.mcp.register` | Declaration | `list`, `tools`, and `status` are reads; candidate reads must not expose uncommitted live services. |
| `smelt.lsp.configure` | Declaration | It currently mutates `LspManager` and is a transaction escape that Phase 3 must remove. |
| `smelt.process.set_default_shell` | Declaration | `get_default_shell` reads candidate declaration. |
| `smelt.mode.register`, `smelt.mode.set_icon` | Declaration | Mode catalog, behavior, icon, and cycle are candidate desired state. `set` and `cycle` are runtime-only UI actions. Other mode helpers are reads. |
| `smelt.reasoning.set`, `smelt.reasoning.cycle` | Runtime-only | Explicit user selection actions. `current` and `cycle_list` are reads. |
| `smelt.settings` assignment | Declaration | `schema` is a pure read. Runtime assignment must write the same desired store and request reconciliation. |
| `smelt.permissions.extend` | Declaration | Static rules. `grant_session` and `sync` are runtime-only live approval operations; `list`, `check`, and `check_tool` are reads. |
| `smelt.tools.register`, `patch`, `unregister`, `middleware` | Generation | Tool callbacks are generation-owned. Tool permission defaults are snapshotted as declarations. Read/format helpers remain pure. |
| `smelt.cmd.register` | Generation | `list` reads candidate registry. `run` and `picker` are runtime-only. |
| `smelt.keymap.set`, `unset`, `set_leader` | Generation | Remaining helpers read candidate keymap declarations. |
| `smelt.lifecycle` | Generation | Ready/shutdown hooks and guards cannot escape a discarded candidate. |
| `smelt.events` | Generation | Event callbacks and event objects are generation-owned. |
| `smelt.signal.new`, `subscribe` | Generation | Signal callbacks are generation-owned. Built-in signal reads are committed-runtime reads. |
| `smelt.timer`, `smelt.tick` | Generation | Timers stay paused until commit and are cancelled on retirement. |
| `smelt.task`, `smelt.spawn` | Generation | Candidate tasks stay paused; completions carry generation identity. |
| `smelt.transcript.set_renderer`, `extend_renderer`, `invalidate_renderer`, `smelt.transcript.groups.register` | Generation | Other transcript/default/group helpers are pure rendering or committed-runtime reads. |

### Launch-only and persistent APIs

| Namespace/function | Class | Notes |
| --- | --- | --- |
| `smelt.cli` | Launch-only | Specs may be compared on reload, while parsed values remain startup inputs. |
| `smelt.builtins` | Launch-only | Autoload selection is fixed before normal candidate loading. |
| `smelt.state` | Persistent | Flush before candidate creation; reads use the committed snapshot. |
| `smelt.phase` | Pure read | Reads candidate generation phase. It is internal and absent from the generated public namespace index. |
| `smelt.frontend`, `smelt.build` | Pure read | Immutable launch metadata. |

### Generation-owned TUI resources

All callbacks attached through the following namespaces are generation-owned.
Pure construction can occur in the candidate, but bindings and visible
resources are staged until commit:

- `smelt.buf`, `smelt.win`, `smelt.overlay`, `smelt.paint`, `smelt.input`,
  `smelt.picker`, `smelt.list`, `smelt.dialog`, `smelt.confirm`
- `smelt.ui`, `smelt.ui.layout`, `smelt.layout`, `smelt.render`,
  `smelt.transcript`, `smelt.transcript.defaults`,
  `smelt.transcript.groups`
- `smelt.prompt` callback/completer/acquire/open-picker surfaces
- `smelt.keymap`, `smelt.theme.apply`, `smelt.theme.set`, and
  `smelt.theme.use`

The read-only formatting, measuring, text extraction, and theme lookup
functions in these namespaces are pure. Prompt edits, picker/dialog opening,
focus changes, and visible UI mutations are runtime-only outside ready hooks;
a candidate may stage ready-hook work but may not show it before commit.

### External effects and runtime-only operations

| Namespace/function group | Class during candidate load |
| --- | --- |
| `smelt.fs` reads, `smelt.path`, `smelt.parse`, `smelt.json`, `smelt.fuzzy`, `smelt.html`, `smelt.text`, `smelt.image` pure transforms | Pure read |
| `smelt.fs` writes/removes/rename/copy/mkdir and async write variants | External effect |
| `smelt.fs.watch` | Generation; subscribe only after commit |
| `smelt.http`, `smelt.auth`, `smelt.grep` | External effect when network/process work starts |
| `smelt.http.cache.read` | Pure read; cache writes are external effects |
| `smelt.process` spawn/run/kill/stop/detach | External effect |
| `smelt.os` metadata/getenv/path reads | Pure read over explicit candidate context or immutable host data |
| `smelt.os.setenv`, `unsetenv`, `set_cwd`, URL opening | External effect |
| `smelt.clipboard` | Runtime-only external effect |
| `smelt.files.search`, `status` | Committed session-service read; `accept` and `rescan` are runtime-only effects |
| `smelt.agent.add_system_prompt` | Declaration feeding future prompt resolution |
| `smelt.skills` | Pure read over candidate target cwd and committed skill inputs |
| `smelt.log` | Deferred candidate diagnostic; do not publish candidate logs as committed behavior before success |
| `smelt.messages` | Runtime-only session service |
| `smelt.trust.mark` | External effect; `status` reads candidate project context |
| `smelt.engine` | Runtime-only, except hook registration which is generation-owned |
| `smelt.model.set`, `smelt.reasoning.set`, `smelt.mode.set` | Runtime-only explicit user actions |
| `smelt.model`, `smelt.config` reads | Read candidate resolved state during ready hooks, committed state otherwise |
| `smelt.session` and child namespaces | Runtime-only session operations and reads |
| `smelt.history`, `smelt.metrics`, `smelt.metrics.perf`, `smelt.search`, `smelt.work`, `smelt.vim` | Runtime-only session/TUI operations |
| `smelt.notify`, `smelt.terminal` | Stage ready-hook output, otherwise runtime-only external effects |
| `smelt.notebook` reads/parsing | Pure read; edits are external effects |
| `smelt.perf`, `smelt.time` | Pure measurement/read |
| `smelt.shell` | Pure parsing/classification |
| `smelt.reg` | Inherits the owner of the registration it wraps |
| `smelt.focus`, `smelt.quit` | Runtime-only |
| `smelt.ns`, `smelt.plugin` | Generation module/namespace construction |
| `smelt.sleep` | Generation task |

This table includes the remaining generated namespaces not called out above:
`smelt.config`, `smelt.history`, `smelt.inspect`, `smelt.model`,
`smelt.notebook`, `smelt.notify`, `smelt.search`, `smelt.session`,
`smelt.session.messages`, `smelt.session.slug`, `smelt.session.title`,
`smelt.terminal`, `smelt.text`, `smelt.vim`, and `smelt.work`.

For machine-checked namespace coverage, the mixed namespaces discussed through
function-level exceptions above are: `smelt`, `smelt.agent`, `smelt.cmd`,
`smelt.files`, `smelt.fs.file_state`, `smelt.http.cache`, `smelt.lsp`,
`smelt.mcp`, `smelt.mode`, `smelt.permissions`, `smelt.reasoning`,
`smelt.signal`, `smelt.theme`, `smelt.tools`, and `smelt.trust`. The xtask test
`hot_reload_audit_classifies_every_generated_namespace` compares this ledger to
the generated API index.

## Active model read inventory

The active model is currently split across `AppConfig.model`, `api_base`,
`api_key_env`, `provider_type`, and `model_config`. The table lists all
production read owners. Tests that assign these fields directly are not runtime
consumers and are intentionally omitted.

| Owner | Reads and purpose |
| --- | --- |
| `src/startup.rs` | Resolves provider/model precedence, managed caches, transport, key env, and `ModelConfig`; constructs `ResolvedStartup`. |
| `src/main.rs` | Projects `ResolvedStartup` into `AppConfig`, `EngineConfig`, dispatcher startup, and initial system-prompt capabilities. |
| `crates/core/src/runtime.rs` | Seeds the built-in model signal from `AppConfig.model`. |
| `crates/core/src/headless_app.rs` | Resolves the API key and builds the headless `StartTurnPayload`. |
| `crates/tui/src/app/agent.rs` | Builds normal/custom `StartTurnPayload`s, resolves custom model overrides, and resolves API keys. The custom override fallback can mix a custom model with active transport/config. |
| `crates/tui/src/commands.rs` | Applies model selection, sends `SetModel`, updates the model signal, and starts context-window discovery. |
| `crates/tui/src/app.rs` | Builds context-token identity, provider kind, stale-result checks, system-prompt capabilities, and API-base warnings. |
| `crates/tui/src/app/history.rs` | Reverse maps active transport/model fields to a persisted model key. |
| `crates/tui/src/app/engine_events.rs` | Attributes usage to the current UI model rather than a request-owned target. |
| `crates/tui/src/lua/api/config.rs` | Exposes active transport and `ModelConfig`. |
| `crates/tui/src/lua/api/model.rs` | Exposes catalog, current model, pricing, token limits, modalities, transport, and capabilities. |
| `crates/tui/src/lua/api/session.rs` | Exposes active model/provider/base in session status/info. |
| `crates/tui/src/lua/api/engine.rs` | Resolves `EngineAsk` model overrides and transport before sending the command. |
| `crates/tui/src/inspect_server.rs` | Serializes persisted request model/base metadata for inspection. |

Protocol/model transport producers and consumers are:

- `protocol::StartTurnPayload` in `crates/protocol/src/event.rs`
- `UiCommand::EngineAsk` in `crates/protocol/src/event.rs`
- custom command overrides in `crates/core/src/custom_commands.rs` and
  `crates/tui/src/app/agent.rs`
- headless production in `crates/core/src/headless_app.rs`
- interactive production in `crates/tui/src/app/agent.rs` and
  `crates/tui/src/lua/api/engine.rs`
- consumption in `crates/engine/src/agent.rs`

## Engine command receive inventory

Every `cmd_rx` receive/drain site is in `crates/engine/src/agent.rs`:

| Site | State | Model-command behavior to preserve or replace |
| --- | --- | --- |
| `engine_task` outer `select!` | Idle | Handles `StartTurn`, `EngineAsk`, and global `SetModel`. |
| `Turn::host_call` | Waiting for a host Lua callback | Defers turn-injection commands and handles other commands. |
| `Turn::execute_concurrent` | Concurrent tool hooks/permissions/execution | Defers model/mode/reasoning commands; dispatches background commands. |
| `Turn::drain_commands` through `handle_turn_cmd` | Between request/tool transitions | Applies pending model/mode/reasoning changes. |
| `Turn::call_llm` | Provider request in flight | Captures the latest `SetModel` and applies it after the request boundary. |
| `Turn::wait_for_tool_result` | Sequential tool result wait | Applies model/mode/reasoning commands while waiting. |

`dispatch_background_cmd` is the common `EngineAsk` handler used by all receive
sites that can accept background work. Phase 1 must keep one centralized
command classifier and replace tuple-style `SetModel` handling with complete
request-owned values.

## Deterministic async test seams

Race tests must control ordering with channels, not sleeps.

| Component | Existing seam or Phase 0 fixture | Required delivery point |
| --- | --- | --- |
| Managed model refresh | `src/startup.rs` has a closure-driven refresh helper used by the ignored characterization test. The fake exposes started/release/completed channels. | Completion must enter the app event loop with request, auth, and desired revisions. |
| MCP | `McpManager::reconcile` is callable with controlled tasks; tests can gate when competing reconcile futures are first polled. Server definitions can be installed without a subprocess in module tests. | A latest-desired controller must serialize publication and reject stale connection completions. |
| LSP | `LspManager::configure` is async and `reconcile_config` is a pure synchronous seam. Controlled configure futures can be gated before polling; client startup publication needs revision checks in the controller. | Desired config and started clients publish only for the latest revision. |
| Auto reload watcher | `debounce_loop_inner` accepts a fake subscription closure and channel-driven filesystem hints. `ConfigSnapshot` is deterministic. | Controller setup completion must carry desired revision before installing a handle. |
| Context window | `TuiApp::apply_context_window_update` already checks request id plus model/base/provider identity. | Replace tuple identity with runtime revision plus complete model target. |

Characterization tests whose desired assertions are known to fail are marked
`#[ignore = "hot reload refactor characterization"]`. They are intentionally
run directly during the phase that fixes their subsystem, then unignored in
that phase. The normal workspace gate therefore remains green while preserving
an executable statement of each defect.
