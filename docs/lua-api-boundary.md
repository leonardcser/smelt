# Lua API Boundary

This document defines the alpha Lua boundary behind `smelt.api_version == "2"`.
Rust declarations and bundled Lua annotations generate the reference docs,
LuaCATS stubs, and completion data. They describe the current API, not a frozen
compatibility inventory.

## Classifications

The classifications are provisional documentation and design guidance during
alpha:

- **Supported** is the preferred plugin and configuration facade. It aims for
  coherent behavior, typed signatures, and consistent ownership conventions.
- **Advanced** is a documented low-level capability for plugins that need direct
  control. It is externally callable but may evolve more freely.
- **Internal** is bundled-runtime machinery. Rust stores these capabilities in a
  VM-private registry tree, and only trusted bundled Lua chunks receive the
  private `__smelt_internal` reference. User and project configuration, plugins,
  runtime overrides, `require`, and `package.loaded` cannot obtain it.

Supported and Advanced do not freeze the current broad surface before beta.
Difficulty or a leading underscore does not make a capability Internal. A useful
low-level extension point remains Advanced; Internal is reserved for machinery
such as raw facade bridges, lifecycle sweep hooks, and candidate scope state.

## Capability map

| Plugin capability | Supported facade | Bundled use case | Advanced escape hatch |
| --- | --- | --- | --- |
| Commands and input actions | `smelt.cmd`, `smelt.keymap`, `smelt.events`, `smelt.lifecycle` | command modules and `plugins/esc_chord.lua` | direct window key/event methods |
| State and reload-safe work | `smelt.state`, `smelt.reg`, `smelt.tick`, `smelt.timer`, `smelt.signal` | model preferences, title, prediction, and status refresh | direct signal construction and low-level timers |
| Async I/O and cancellation | `smelt.spawn`, `smelt.task`, `smelt.fs`, `smelt.http`, `smelt.process` | LSP startup, compact requests, and filesystem tools | raw task and process control where documented |
| Providers, tools, and permissions | `smelt.provider`, `smelt.tools`, `smelt.permissions`, `smelt.engine` hooks | compact middleware and bundled edit/write/notebook tools | provider row helpers and tool watchdog/compaction helpers |
| Composable terminal UI | `smelt.dialog`, `smelt.input`, `smelt.list`, `smelt.picker`, `smelt.ui.layout`, `smelt.render` | confirm, picker, completer, and inspect UI | `smelt.buf`, `smelt.win`, `smelt.overlay`, `smelt.paint`, and `smelt.confirm.open` |
| Transcript presentation | semantic `smelt.transcript` views, targets, renderer extensions, groups, and tool presentation | default transcript renderer and inspect views | direct buffer/window rendering primitives |
| Session and user configuration | `smelt.settings`, `smelt.session`, `smelt.model`, `smelt.mode`, `smelt.reasoning` | bundled mode, title, predict, and compact plugins | low-level config/runtime status snapshots |

Bundled plugins use the Supported facade whenever external plugins have the same
need. Private capabilities are limited to runtime commit, rollback, scope, and
teardown work that user code must not invoke.

## Runtime tiers

`Host` functions work in core and headless runtimes. `UiHost` functions require
an active terminal UI scope and return a Lua runtime error of the form
`<api> requires an active terminal UI` when it is absent. Startup and candidate
loading receive UiHost access only while the application has actually entered
that scope.

## Candidate evaluation

smelt evaluates a replacement Lua generation before committing it. Declarations
and resources owned by smelt's generation machinery are replaced together on
success; a failed candidate is discarded while the committed generation remains
active.

Operations that directly affect the live application or its environment are
guarded at their Rust or private-runtime effect boundary. Such operations return
an unavailable-during-candidate error until the replacement generation commits.
Pure composition does not need a separate public policy: if it reaches a guarded
effect, that effect rejects the call.

This mechanism is not a Lua sandbox or a general transaction manager. Config and
plugins are trusted in-process code. Arbitrary filesystem, process, network, or
other external effects performed outside smelt-managed declarations are not
rolled back when candidate evaluation fails.

## Values and failures

Supported APIs aim to follow these conventions:

- Collections and cursor positions document whether they are 0-based byte
  offsets or 1-based Lua rows/items. Buffer text mutations snap stale offsets at
  UTF-8 boundaries.
- Omitted optional values and explicit `nil` have the same meaning unless the
  API documents setter/clear overloads, such as `smelt.model.preferred`.
- Invalid arguments, unavailable runtime capabilities, and violated preconditions
  raise Lua errors.
- Fallible synchronous and coroutine I/O returns `(value, nil)` on success and
  `(nil, message)` on operational failure. APIs expose an additional status,
  code, or result field when callers need to branch without parsing a message.
  A completed process with a non-zero exit remains a process result, not a host
  failure.
- Tool execution returns `{ content, is_error, metadata? }` because a tool error
  is a model-visible domain result rather than a Lua transport failure.
- Coroutine cancellation raises `cancelled`. Callers that catch failures use
  `smelt.task.is_cancelled(err)` instead of parsing strings; the helper accepts
  both coroutine cancellation and typed engine cancellation records.

## Registration and ownership

`smelt.Reg` is the common ownership handle. The first `:remove()` performs
cleanup and returns `true`; later calls are inert and return `false`. A plugin can
combine handles with `smelt.reg.compose` or adapt custom teardown with
`smelt.reg.new`.

Registrations managed by the runtime belong to the Lua generation that created
them. Successful reload rebuilds declarations in a candidate and retires
registrations absent from the committed replacement. Failed reload discards the
candidate declarations. Shutdown cancels tasks and timers, flushes persistent
state, and removes generation-owned callbacks and resources.

Named UI resources use their documented name as a reuse key. Redeclaration
refreshes the existing logical resource; omission from the next committed
generation retires it. Anonymous resources are generation-owned and are reaped.
After removal, registration handles are inert, cancelled tasks cannot publish
results, and stale callbacks are not invoked.

## Alpha version policy

Version 2 is the greenfield alpha API. Breaking alpha cleanup updates the current
declarations and generated artifacts in place. Supported and Advanced guide API
review, but neither classification freezes the current surface or requires a
cross-base compatibility check. Do not create version 3 solely to record
pre-beta iteration. A future beta policy can define a narrower compatibility
promise and release-note process.
