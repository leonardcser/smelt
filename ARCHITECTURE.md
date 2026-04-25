# Architecture

How smelt is structured, why it's structured that way, and the rules we follow
when changing it. The detailed migration history that produced this shape is in
commit messages — this doc describes the current target, not the path we took.

## North star

Three commitments. Every change lands closer to all three, never farther from
any.

1. **One rendering path.** The compositor (`ui::Ui`) owns every pixel. No
   parallel ANSI emitter, no cached "last seen" state outside the grid diff.
   Anything visible is a window registered with the compositor.

2. **FFI = internal API.** Lua plugins call the same Rust functions the core
   does. `smelt.*` is a thin wrapper, not a translation layer. No stringly-typed
   action tokens. Behaviour is expressed as callbacks (`Callback::Rust` /
   `Callback::Lua`), not strings the host matches on.

3. **Rust core, Lua features.** Rust owns pixel pushing (compositor, buffers,
   windows, widgets, rendering) and security-critical tools (bash, read, write,
   edit, glob, grep, session/agent lifecycle). Lua plugins own the _what_: which
   dialogs, tools, and slash commands exist, and how their panels compose. Same
   model as Neovim — small C core, everything user-facing is a plugin. **A
   feature living in Rust that could live in Lua is a bug.**

## Core model

Two primitives, same as Neovim.

- **`ui::Buffer`** — content + metadata. Lines, highlights, decorations, marks,
  virtual text, modifiable flag. Knows nothing about display.
- **`ui::Window`** — viewport into a buffer. Cursor, scroll, selection, vim
  state, kill ring, keybindings, mouse handling, `tail_follow`, `modifiable`.
  The transcript window and the prompt window are the same kind of thing — they
  only differ in their buffer's `modifiable` flag.

Above the two primitives sit four named, opinionated components. Same paint
surface, different interaction contracts:

| Component      | Role                                                | Focusable | Placement                   |
| -------------- | --------------------------------------------------- | --------- | --------------------------- |
| `Dialog`       | Modal panel stack (resume, agents, confirm, …)      | yes       | `dock_bottom` / centered    |
| `Picker`       | Non-focusable dropdown, externally-driven selection | no        | `prompt_docked` (auto-flip) |
| `Cmdline`      | `:` prompt, single-line, owns its cursor            | yes       | docked above status         |
| `Notification` | Ephemeral toast                                     | no        | anchored above prompt       |

Layout decides where surfaces sit; the focus graph decides which windows
`<C-w>w`-style cycling visits. Splits are always focusable; floats opt in via
`WinConfig.focusable` (mirrors Neovim).

## The Lua FFI contract

Lua bindings reach `&mut App` directly through a TLS pointer
(`crates/tui/src/lua/app_ref.rs`):

```rust
crate::lua::with_app(|app| app.do_thing());          // panics if missing
crate::lua::try_with_app(|app| app.read_thing())     // None if missing
    .unwrap_or_default();
```

`install_app_ptr(self)` is set at every `&mut App` site that drives Lua: the
main loop tick, startup `load_plugins`, and the deferred callback drain. Lua
callbacks registered via `ui::Callbacks` would collide with the `&mut Ui` borrow
held during `ui.handle_key`, so they're queued (`pending_invocations`) and
drained after the borrow releases.

**No effect log, no closure queue, no snapshot mirror.** Reads go live
(`try_with_app`); writes go direct (`with_app`); `LuaShared` holds only genuine
Lua-runtime state (handle registries, atomic counters, the coroutine runtime,
the deferred-invocation queue).

## What lives where

| Concern                                                       | Owner                                       |
| ------------------------------------------------------------- | ------------------------------------------- |
| Pixel pushing (grid, diff, SGR)                               | `ui::`                                      |
| Buffer / Window primitives                                    | `ui::`                                      |
| `Dialog` / `Picker` / `Cmdline` / `Notification` components   | `ui::`                                      |
| Compositor event routing (z-ordered keys, hit-tested mouse)   | `ui::Ui`                                    |
| Fuzzy match (`fuzzy.score`, `fuzzy.rank`)                     | Rust core, exposed to Lua                   |
| Theme roles, color resolution                                 | Rust core, exposed via `smelt.theme.*`      |
| Security-critical tools (bash, read, write, edit, glob, grep) | Rust core                                   |
| Session / agent / engine lifecycle                            | Rust core                                   |
| Slash commands (`/model`, `/theme`, `/resume`, `/export`, …)  | Lua plugins                                 |
| Dialogs (confirm, permissions, agents, rewind, …)             | Lua plugins                                 |
| Plugin tools                                                  | Lua plugins                                 |
| Statusline content                                            | Lua sources via `smelt.statusline.register` |

**Test for "Rust core" vs "Lua feature":** would a _different_ plugin reuse this
exact code? Yes → Rust primitive. No → Lua feature.

## Coroutine-based async

One suspend mechanism: `mlua::Thread`. Plugin code reads top-to-bottom and looks
synchronous:

```lua
smelt.spawn(function()
  local result = smelt.prompt.open_picker({ items = ... })
  if result then act_on(result.item) end
end)
```

Yielding from a non-task context is a Lua error with a clear message. The
runtime drives parked tasks each tick; resumption delivers the awaited payload.
ID plumbing is internal — plugins never see request ids, callback registries, or
completion ports.

## Why not ratatui

- Immediate-mode rebuilds every frame; we want grid diffing.
- No persistent windows with cursor / scroll / focus.
- No z-order beyond render order.
- `ratatui::Buffer` = cell grid; our `ui::Buffer` = content model.

We borrow only the cell-grid concept as the intermediate rendering surface.

## Why `ui` is a separate crate

- Forces clean boundaries — `ui` cannot import `protocol::` or `engine::`.
- Testable in isolation.
- The public API surface _is_ the API. `pub` items in `ui` are the contract.

## Rules of engagement

These are non-negotiable for any change touching architecture.

### Process

- **Green tree every commit.** Run before each commit:
  `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo nextest run --workspace`.
- **Atomic refactors.** Don't ship intermediary scaffolding — parallel trait
  impls, stringly-typed bridges, "kept for now" stubs that get deleted next
  commit. A larger diff that lands the tree in its final shape beats a chain of
  small diffs that leave it half-migrated.
- **One commit = one architectural move.** A migration that deletes the old
  surface alongside the new is one commit. Adding the new while leaving the old
  in place is incomplete.
- **Plans bend, code ships.** When friction shows up, stop, pick a better path,
  keep going. The artefact that matters is the code, not the path we took.
- **No cost calculus on effort.** Refactor size, time-to-land, and migration
  difficulty are not trade-offs. Only outcome quality counts: correctness,
  modularity, simplicity, maintainability.

### Code

- **Neovim-inspired first.** When in doubt, do what Neovim does. Deviate only if
  it improves the outcome, and say why.
- **No `#[allow(dead_code)]`.** Use it, remove it, or leave the warning as a
  tracking marker.
- **No throwaway work.** Every step is a subset of the final state. If you're
  writing code you know you'll delete next commit, fold the deletion into this
  commit.
- **No stringly-typed dispatch.** `Callback::Lua` / `Callback::Rust` are the
  behaviour mechanism. `KeyResult::Action(String)` survives inside `ui` only as
  the widget→container internal protocol.
- **No state mirrors in `LuaShared`.** App state stays on App and is read live
  via `with_app`. `LuaShared` holds only genuine Lua-runtime state.
- **Comments answer "why", not "what".** Default to writing none. Don't
  reference the current task / fix / caller — that belongs in the commit
  message.

### Testing TUI changes

`cargo nextest run` covers unit-testable logic. For visual behaviour (dialog
rendering, layout, selection), drive the real binary in a tmux side pane.
**You're already inside tmux** — never `tmux kill-server` / `kill-session` /
`new-session`. Split the current window:

```bash
tmux split-window -h -t <session:window> -c <worktree-path> -P -F '#{pane_id}'
tmux send-keys -t %ID './target/debug/smelt' Enter
tmux capture-pane -t %ID -p | tail -N
```

Stop with `tmux send-keys -t %ID C-c`. Close the pane with
`tmux kill-pane -t %ID`.

Debug with `eprintln!` — stderr lands in `~/.local/state/smelt/logs/stderr.log`.
Tail it from another shell. Strip the `eprintln!`s before committing.

### UI conventions

- Dialog titles are lowercase (`resume (workspace):`, `permissions`). Uppercase
  is reserved for proper nouns.
- Meta columns are `dim`, content is normal weight. Selection retints the whole
  row fg.
- Selection = fg-accent on the cursor row. No bg fill, no cursor glyph, no
  layout shift.
- Every dialog reserves one blank row between panel content and the hints row.

## Assumptions

- Single-threaded TUI loop. The Lua runtime is `!Send` (mlua holds Lua thread
  state); cross-thread state crosses through `Arc<Mutex<…>>` boundaries (engine
  sender, agent registries, process registry, agent_snapshots).
- The TLS app pointer is sound because Rust never touches its `&mut App` borrow
  while Lua is executing — the FFI call is synchronous on a single thread, so
  the reborrow inside `with_app` is sole.
- Plugin authors trust the snapshot semantics: reads observe current App state
  at read time. There is no per-tick freeze.
- Embedded Lua modules under `runtime/lua/smelt/` are the source of truth for
  autoloaded plugins. User `~/.config/smelt/init.lua` runs after autoloads and
  can override anything.
