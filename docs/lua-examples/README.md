# Lua plugin examples

Drop any of these files into `~/.config/smelt/init.lua` (or `dofile` them from your own `init.lua`) to try them out.

- **per_project.lua** — auto-load `$PWD/.smelt/init.lua` on top of the user config.
- **mode_keybinds.lua** — `<C-y>` copies transcript or prompt depending on focused window, demonstrating `smelt.api.win.focus()` for context-aware keybinds.
- **yank_block.lua** — `<Space>y` yanks the block under the cursor using `/yank-block`.
- **statusline.lua** — custom status bar showing the current directory path, git branch, and clock via `smelt.statusline(fn)`.
- **override.lua** — register a custom command (`/hello`) and remap a keybind (`<C-s>` to `/fork`).

## API surface

The full Lua API is documented in `crates/tui/src/lua.rs`.

### Core

- `smelt.api.version` — API version string
- `smelt.notify(msg)` — show a notification
- `smelt.clipboard(text)` — copy to system clipboard
- `smelt.keymap(mode, chord, fn)` — register a keymap (`"n"`, `"i"`, `"v"`, or `""` for any mode)
- `smelt.on(event, fn)` — register an autocmd handler
- `smelt.defer(ms, fn)` — schedule a one-shot timer
- `smelt.statusline(fn)` — register a custom status line provider

### Commands

- `smelt.api.cmd.register(name, fn)` — register a user command
- `smelt.api.cmd.run(line)` — execute a command
- `smelt.api.cmd.list()` — list registered command names

### Buffer / Window

- `smelt.api.transcript.text()` — transcript content (snapshot)
- `smelt.api.buf.text()` — prompt buffer content (snapshot)
- `smelt.api.win.focus()` — `"transcript"` or `"prompt"`
- `smelt.api.win.mode()` — vim mode string (`"Normal"`, `"Insert"`, `"Visual"`)

### Engine

- `smelt.api.engine.model()` / `set_model(key)` — current model
- `smelt.api.engine.mode()` / `set_mode(mode)` — agent mode (`"normal"`, `"plan"`, `"apply"`, `"yolo"`)
- `smelt.api.engine.reasoning_effort()` / `set_reasoning_effort(level)` — reasoning level
- `smelt.api.engine.is_busy()` — whether an agent turn is running
- `smelt.api.engine.cost()` — session cost in USD
- `smelt.api.engine.context_tokens()` — prompt tokens used (nil if unknown)
- `smelt.api.engine.context_window()` — context window size (nil if unknown)
- `smelt.api.engine.cancel()` — cancel the running turn
- `smelt.api.engine.compact(instructions?)` — compact conversation history
- `smelt.api.engine.submit(text)` — queue a user message for submission

### Events

Register handlers with `smelt.on(event, fn)`. Simple events pass `(event_name)`, data events pass `(event_name, data_table)`.

| Event | Data | Description |
|---|---|---|
| `block_done` | — | A rendered block finished |
| `cmd_pre` | — | Before a command runs |
| `cmd_post` | — | After a command completes |
| `session_start` | — | Session loaded |
| `shutdown` | — | App is quitting |
| `turn_start` | — | Agent turn dispatched |
| `turn_end` | `{ cancelled }` | Agent turn completed |
| `mode_change` | `{ from, to }` | Agent mode changed |
| `model_change` | `{ from, to }` | Model changed |
| `tool_start` | `{ tool, args }` | Tool execution started |
| `tool_end` | `{ tool, is_error, elapsed_ms }` | Tool execution finished |
| `input_submit` | `{ text }` | User submitted a message |
