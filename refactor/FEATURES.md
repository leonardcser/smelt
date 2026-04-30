# Features — parity checklist

Every user-facing feature smelt has today. The "no feature gets dropped" rule is
enforced by walking this list at every phase boundary and after the end-to-end
parity walk in P7.

Maintenance rules:

- **At every phase boundary**, walk the relevant rows and update **Status**.
- **A feature can be `offline` mid-refactor** — that's fine — but it must return
  to `working` before its phase's end-of-phase summary.
- **A feature whose status is `working` at the end of P7** is the gate for
  completion.
- New features added during the refactor (rare; we're not building, we're
  rebuilding) get a row.

Legend for **Status**:

- `working` — feature works as it does today.
- `offline-Pn` — currently broken because we're mid-phase Pn; expected to return
  by phase X (see "Restored by" column).
- `verified-Pn` — actively walked through and confirmed working at end of phase
  Pn.
- `regressed` — was working, broke; needs a fix before phase boundary.

Source columns are evidence trails — when verifying, read here to remember _how
to invoke_ the feature.

---

## Core agentic features

| Feature                                                       | Source today                                              | Restored by                   | Status  |
| ------------------------------------------------------------- | --------------------------------------------------------- | ----------------------------- | ------- |
| LLM session (start, stream, complete)                         | `engine/agent.rs`, `engine/provider/*`                    | P2 (EngineBridge wired)       | working |
| Model selection (`/model`)                                    | `runtime/lua/smelt/plugins/model.lua`                     | P4.e                          | working |
| Provider: Anthropic                                           | `engine/provider/anthropic.rs`                            | n/a (kept)                    | working |
| Provider: OpenAI                                              | `engine/provider/openai.rs`                               | n/a                           | working |
| Provider: Codex                                               | `engine/provider/codex.rs`                                | n/a                           | working |
| Provider: Copilot                                             | `engine/provider/copilot.rs`                              | n/a                           | working |
| Provider: OpenAI-compatible (Ollama, vLLM, llama.cpp, Gemini) | `engine/provider/mod.rs` (Local)                          | n/a                           | working |
| Reasoning effort cycle (Off/Low/Med/High/Max)                 | `--reasoning-effort` + Ctrl+T + `plugins/toggles.lua`     | P2.c (cell), P4.f (modes)     | working |
| Agent modes (Normal/Plan/Apply/Yolo)                          | `--mode` + Shift+Tab + `protocol/mode.rs`                 | P5 (mode gating to Lua hooks) | working |
| Turn streaming                                                | `engine/agent.rs` → `EngineEvent::TextDelta` → transcript | P2.d (EngineBridge)           | working |
| Steer / Unsteer (queue messages mid-turn) | `protocol/event.rs::Steered` + `UiCommand::{Steer,Unsteer}` + `app/events.rs` | P2.d | working |
| Auto-retry on transient errors (`Retrying`) | `engine/agent.rs` → `EngineEvent::Retrying` + `app/working.rs` spinner | P2.d (EngineBridge fires) | working |
| Auxiliary model routing (title, prediction, btw, compaction) | `engine/agent.rs` + `protocol/usage.rs::AuxiliaryTask` + auxiliary-model config | P2 (kept), P4 (predict/btw plugins) | working |
| Per-turn message snapshot (`Messages` event for transcript sync) | `protocol/event.rs::Messages` + `app/transcript_model.rs` | P2.d | working |
| Plugin tool hook flow (`needs_confirm` / `preflight` / `approval_patterns`) | `protocol/event.rs::EvaluatePluginToolHooks` + `lua/tasks.rs` PluginToolEnv | P5.b (Lua hook fn returns "allow"/"needs_confirm"/"deny") | working |
| Tool call lifecycle states (`ToolStarted` / `ToolOutput` / `ToolFinished` / `ToolStatus::Denied`) | `protocol/event.rs` + `app/transcript_present/tools.rs` | P4.b (Lua presentation) | working |
| Per-turn telemetry (`TurnMeta`, `agent_blocks`, `AgentToolData`) | `protocol/usage.rs` + `session.rs` | P2.a (Session). `AgentBlockData` deleted in P5.c — sub-agent output becomes ordinary tool blocks. | working |
| Cost tracking                                                 | `app/working.rs` + `session.rs` cost fields               | P2.a (Session)                | working |
| Token usage display                                           | `protocol/usage.rs` + status bar                          | P2.c (`tokens_used` cell)     | working |
| Tokens/sec readout                                            | `show_tps` setting + status bar                           | P4.c                          | working |
| History compaction (`/compact`)                               | `app/commands.rs::cmd_compact` + `engine/compact.rs`      | P4.e                          | working |
| Title generation                                              | `EngineEvent::TitleGenerated` + `working.rs`              | P2.c (`session_title` cell)   | working |
| `/btw` side question                                          | `runtime/lua/smelt/plugins/btw.lua`                       | P4.e                          | working |
| File attachment (`@path`)                                     | `attachment.rs` + `input/completer_bridge.rs`             | P1/P4 (extmark + recipe)      | working |
| Image attachment (Cmd+V paste, `read_file` of image)          | `engine/image.rs` + `engine/tools/read_file.rs`           | P5.b                          | working |
| Ghost-text prediction                                         | `runtime/lua/smelt/plugins/predict.lua`                   | P1.d (extmark)                | working |
| Prompt history (↑/↓)                                          | `input/history.rs`                                        | P2                            | working |
| Reverse history search (Ctrl+R, `/history`)                   | `plugins/history_search.lua`                              | P4.e                          | working |
| Input stash (Ctrl+S)                                          | `input/mod.rs`                                            | P1/P4                         | working |
| Multi-agent: spawn                                            | `engine/tools/spawn_agent.rs`                             | P5.b                          | working |
| Multi-agent: stop                                             | `engine/tools/stop_agent.rs`                              | P5.b                          | working |
| Multi-agent: message                                          | `engine/tools/message_agent.rs`                           | P5.b                          | working |
| Multi-agent: peek                                             | `engine/tools/peek_agent.rs`                              | P5.b                          | working |
| Multi-agent: list                                             | `engine/tools/list_agents.rs`                             | P5.b                          | working |
| MCP servers                                                   | `engine/mcp/`                                             | n/a (kept)                    | working |
| Skills loader                                                 | `engine/skills.rs` + `tools/load_skill.rs`                | P5.b                          | working |

## Tools (built-in)

| Tool                  | Source today                      | Restored by                                         | Status  |
| --------------------- | --------------------------------- | --------------------------------------------------- | ------- |
| `read_file`           | `engine/tools/read_file.rs`       | P5.b → `tools/read_file.lua`                        | working |
| `write_file`          | `engine/tools/write_file.rs`      | P5.b                                                | working |
| `edit_file`           | `engine/tools/edit_file.rs`       | P5.b                                                | working |
| `edit_notebook`       | `engine/tools/notebook.rs`        | P5.b                                                | working |
| `bash`                | `engine/tools/bash.rs`            | P5.b                                                | working |
| `run_in_background` (bash flag, not a tool) | `plugins/background_commands.lua` (overrides bash registration) | P5.b → flag on `tools/bash.lua` | working |
| `read_process_output` | `plugins/background_commands.lua` | P5.b                                                | working |
| `stop_process`        | `plugins/background_commands.lua` | P5.b                                                | working |
| `glob`                | `engine/tools/glob.rs`            | P5.b                                                | working |
| `grep`                | `engine/tools/grep.rs`            | P5.b                                                | working |
| `web_fetch`           | `engine/tools/web_fetch.rs`       | P5.b                                                | working |
| `web_search`          | `engine/tools/web_search.rs`      | P5.b                                                | working |
| `ask_user_question`   | `plugins/ask_user_question.lua`   | P5.b → `tools/ask_user_question.lua`                | working |
| `spawn_agent`         | `engine/tools/spawn_agent.rs`     | P5.b                                                | working |
| `list_agents`         | `engine/tools/list_agents.rs`     | P5.b                                                | working |
| `message_agent`       | `engine/tools/message_agent.rs`   | P5.b                                                | working |
| `peek_agent`          | `engine/tools/peek_agent.rs`      | P5.b                                                | working |
| `stop_agent`          | `engine/tools/stop_agent.rs`      | P5.b                                                | working |
| `load_skill`          | `engine/tools/load_skill.rs`      | P5.b                                                | working |
| `exit_plan_mode`      | `plugins/plan_mode.lua`           | P5.b → `tools/exit_plan_mode.lua`                   | working |

## Slash commands

| Command                                           | Source today                      | Restored by                      | Status  |
| ------------------------------------------------- | --------------------------------- | -------------------------------- | ------- |
| `/clear`, `/new`                                  | `runtime/lua/smelt/plugins/session.lua` | P4.e                       | working — Lua command (`04f6419`); calls `smelt.session.reset()` |
| `/quit`, `/exit`, `:q`, `:qa`, `:wq`, `:wqa`      | `runtime/lua/smelt/plugins/quit.lua` | P4.e                          | working — Lua command (`7c028d8`); flips `pending_quit` via `smelt.quit()` |
| `/rewind`                                         | `plugins/rewind.lua`              | P4.d → `dialogs/rewind.lua`      | working |
| `/resume`                                         | `plugins/resume.lua`              | P4.d → `dialogs/resume.lua`      | working |
| `/compact [instructions]`                         | `app/commands.rs::cmd_compact`    | P4.e                             | working |
| `/fork`, `/branch`                                | `runtime/lua/smelt/plugins/session.lua` | P4.e                       | working — Lua command (`04f6419`); calls `smelt.session.fork()` |
| `/model [provider/model]`                         | `plugins/model.lua`               | P4.e                             | working |
| `/settings`                                       | `plugins/settings.lua`            | P4.e                             | working |
| `/theme [name]`                                   | `plugins/theme.lua`               | P4.e + `colorschemes/`           | working |
| `/color [name]`                                   | `plugins/color.lua`               | P4.e                             | working |
| `/stats`                                          | `runtime/lua/smelt/plugins/stats.lua` | P4.e                         | working — Lua command (`11bd6c6`); `smelt.ui.dialog.open` over `smelt.metrics.stats_text`; q / ? / Esc dismiss |
| `/cost`                                           | `runtime/lua/smelt/plugins/stats.lua` | P4.e                         | working — Lua command (`11bd6c6`); `smelt.metrics.session_cost_text` |
| `/export`                                         | `plugins/export.lua`              | P4.e                             | working |
| `/vim`                                            | `plugins/toggles.lua`             | P4.e                             | working |
| `/thinking`                                       | `plugins/toggles.lua`             | P4.e                             | working |
| `/permissions`                                    | `plugins/permissions.lua`         | P4.d → `dialogs/permissions.lua` | working |
| `/ps`                                             | `plugins/background_commands.lua` | P4.e                             | working |
| `/agents`                                         | `plugins/agents.lua`              | P4.d → `dialogs/agents.lua`      | working |
| `/btw <q>`                                        | `plugins/btw.lua`                 | P4.e                             | working — Overlay (P1.c C.6) |
| `/help`                                           | `plugins/help.lua`                | P4.e                             | working — Overlay (P1.c C.6) |
| `/history`                                        | `plugins/history_search.lua`      | P4.e                             | working |
| `/yank-block` (opt-in)                            | `plugins/yank_block.lua`          | P4                               | working |
| `/reflect`                                        | `builtin_commands.rs`             | P4.e                             | working |
| `/simplify`                                       | `builtin_commands.rs`             | P4.e                             | working |
| Custom commands (`~/.config/smelt/commands/*.md`) | `custom_commands.rs`              | P4.e                             | working |
| `! <shell>` (shell escape)                        | `app/cmdline.rs`                  | P4 (cmdline widget)              | working |

## Dialogs / interactive surfaces

| Dialog                                             | Source today                                                    | Restored by                     | Status  |
| -------------------------------------------------- | --------------------------------------------------------------- | ------------------------------- | ------- |
| Confirm dialog (tool approval, Tab to add message) | `app/dialogs/confirm.rs` + `lua/confirm_ops.rs` + `confirm.lua` | P4.d → `dialogs/confirm.lua`    | working |
| Diff preview pane in confirm                       | `app/dialogs/confirm_preview.rs` + `lua/render_ops.rs`          | P4.b (`diff.lua` extmarks)      | working |
| Permissions picker                                 | `plugins/permissions.lua`                                       | P4.d                            | working |
| Agents picker (list + detail)                      | `plugins/agents.lua`                                            | P4.d                            | working |
| Rewind picker                                      | `plugins/rewind.lua`                                            | P4.d                            | working |
| Resume picker (workspace toggle)                   | `plugins/resume.lua`                                            | P4.d                            | working |
| Model picker                                       | `plugins/model.lua`                                             | P4.d/e                          | working |
| Theme picker (live preview)                        | `plugins/theme.lua`                                             | P4.d                            | working |
| Color picker                                       | `plugins/color.lua`                                             | P4.d/e                          | working |
| Settings menu                                      | `plugins/settings.lua`                                          | P4.d/e                          | working |
| ask_user_question dialog (1-4 options)             | `plugins/ask_user_question.lua`                                 | P5.b                            | working |
| Export dialog (clipboard / file)                   | `plugins/export.lua`                                            | P4.e                            | working |
| Help dialog                                        | `plugins/help.lua`                                              | P4.e                            | working — Overlay (P1.c C.6); Esc dismisses |
| `/btw` streaming-answer dialog                     | `plugins/btw.lua` (`smelt.ui.dialog.open` + spinner-driven content buf) | P4.e                  | working — Overlay (P1.c C.6); Esc dismisses |
| Process picker (`/ps`)                             | `plugins/background_commands.lua`                               | P4.e                            | working |
| History search picker (Ctrl+R)                     | `plugins/history_search.lua`                                    | P4.e                            | working |
| Cmdline (`:` prompt) with completer                | `app/cmdline.rs` + `completer/*`                                | P4 → `widgets/cmdline.lua`      | working |
| Notification toast                                 | `ui/notification.rs` + `smelt.notify`                           | P4 → `widgets/notification.lua` | working |

## Keyboard / mouse interactions

| Behavior                                        | Source today                            | Restored by                                      | Status  |
| ----------------------------------------------- | --------------------------------------- | ------------------------------------------------ | ------- |
| Vim Insert / Normal / Visual / VisualLine       | `ui/vim/*` + `keymap.rs`                | P1.d (decompose to App+Buffer+Window)            | working |
| Vim motions (h/j/k/l, w/b/e, ^/$, gg/G, %, f/t/F/T, ;/,) | `ui/vim/motions.rs` | P1.d (recipe on Window) | working |
| Vim text objects (`iw`/`aw`, `i"`/`a"`, `i(`/`a(`, etc.) | `ui/vim/text_objects.rs` | P1.d | working |
| Vim operators (d, c, y + linewise D/C/Y) | `ui/vim/mod.rs` | P1.d | working |
| Vim Visual anchor flip (`o`) | `ui/vim/mod.rs` | P1.d | working |
| Vim case toggle (`~` in Normal/Visual; `U`/`u` in Visual)         | `ui/vim/mod.rs` (single-char in Normal at line 581; selection toggle in Visual at line 1137) | P1.d | working |
| Emacs-style word case (Alt+U / Alt+L / Alt+C) | `keymap.rs::KeyAction::{UppercaseWord, LowercaseWord, CapitalizeWord}` | P1.d | working |
| Vim dot-repeat (`.`) | `ui/vim/mod.rs` (per-buffer history) | P1.a (moves to Buffer) | working |
| Vim toggle (`/vim` or config)                   | `plugins/toggles.lua`                   | P4.e                                             | working |
| Select character (Shift+arrows)                 | `ui/window.rs` selection                | P1.d                                             | working |
| Select word (Shift+Alt/Ctrl+arrows)             | `ui/window.rs`                          | P1.d                                             | working |
| Select to line boundary (Shift+Home/End)        | `ui/window.rs`                          | P1.d                                             | working |
| Copy (Cmd+C)                                    | `kill_ring.rs` + `clipboard.rs`         | P2 (Clipboard subsystem)                         | working |
| Cut (Cmd+X)                                     | `kill_ring.rs`                          | P2                                               | working |
| Undo (Ctrl+\_)                                  | `ui/undo.rs`                            | P1.a (Buffer undo)                               | working |
| Redo (vim Ctrl+R, normal)                       | `ui/vim/*`                              | P1.d                                             | working |
| Yank (`y`/`yy`, Ctrl+Y emacs)                   | `ui/vim/*` + `kill_ring.rs`             | P1.d/P2                                          | working |
| Paste (`p`/`P`, Ctrl+Y emacs)                   | `ui/vim/*` + clipboard                  | P1.d                                             | working |
| Kill ring rotate (Alt+Y)                        | `kill_ring.rs`                          | P2                                               | working |
| Cursor by character (Ctrl+F/B, arrows)          | `ui/window.rs`                          | P1.d                                             | working |
| Cursor by word (Alt+F/B, Ctrl+arrows)           | `ui/window.rs`                          | P1.d                                             | working |
| Buffer start/end (Cmd+Up/Down)                  | `ui/window.rs`                          | P1.d                                             | working |
| Mode cycle (Shift+Tab)                          | `keymap.rs` + `app/commands.rs`         | P4.f                                             | working |
| Reasoning cycle (Ctrl+T)                        | `keymap.rs`                             | P4.f                                             | working |
| Ghost-text accept (Tab)                         | `completer/mod.rs` + `predict.lua`      | P1.d (extmark)                                   | working |
| Submit (Shift+Enter for multiline)              | `input/mod.rs`                          | P4 (input widget)                                | working |
| Cancel (Ctrl+C)                                 | `app/events.rs` + `engine/cancel.rs`    | P6                                               | working |
| Double-Esc (cancel + drain queue)               | `app/events.rs`                         | P6 (Esc chain)                                   | working |
| Mouse wheel scroll                              | `app/mouse.rs`                          | P1/P2                                            | working |
| Mouse click focus                               | `app/mouse.rs`                          | P2.b (HitTarget + Host)                          | offline-P0 |
| Mouse click position cursor                     | `app/mouse.rs` + `ui/window.rs`         | P1.d                                             | working |
| Drag-extend selection (auto-copy on release)    | `ui/window.rs` + `app/mouse.rs`         | P1 (host.clipboard from Window::handle)          | offline-P0 |
| Double-click WORD yank (vim W: whitespace-delimited, punctuation in) | `ui/window.rs` (`select_big_word_at_transparent`) | P1 (host.clipboard)                              | offline-P0 |
| Triple-click line yank                          | `ui/window.rs` (`select_line_at`)       | P1 (host.clipboard)                              | offline-P0 |
| Scrollbar drag                                  | `app/mouse.rs` + `content/scrollbar.rs` | P2.b (HitTarget::Scrollbar)                      | working |
| Edge autoscroll on drag                         | `app/mouse.rs`                          | P2 (Timers)                                      | working |
| Tab cycles focus (modal-aware)                  | `app/pane_focus.rs`                     | P1.f (`focus_next` modal-aware)                  | working |
| Esc chain (clear sel → dismiss → cancel)        | `app/events.rs` + `dialog.rs`           | P6                                               | working |
| Picker navigation (↑/↓/j/k/Ctrl+P/N, PgUp/PgDn) | `ui/picker.rs` + `option_list.rs`       | P4 (`widgets/picker.lua`, `widgets/options.lua`) | working |
| Picker filter typing                            | `ui/picker.rs`                          | P4                                               | working |
| Custom keymaps (Lua `smelt.keymap.set`)         | `lua/api/dispatch.rs`                   | P3.b                                             | working |

## Theming & UI customization

| Feature                                               | Source today                      | Restored by                 | Status  |
| ----------------------------------------------------- | --------------------------------- | --------------------------- | ------- |
| Theme accent presets (12 colors: ember, coral, rose, gold, ice, sky, blue, lavender, lilac, mint, sage, silver) | `plugins/theme.lua` + `theme.rs::PRESETS` | P1.0 + P4 (`colorschemes/`) | working |
| Custom ANSI accent (0-255)                            | `theme.rs`                        | P1.0 (registry)             | working |
| Task slug accent                                      | `plugins/color.lua`               | P4                          | working |
| `show_tokens` setting                                 | `state.rs::ResolvedSettings`      | P2.a (AppConfig)            | working |
| `show_cost` setting                                   | settings                          | P2.a                        | working |
| `show_tps` setting                                    | settings                          | P2.a                        | working |
| `task_slug` setting                                   | settings + status bar             | P2.a/P4.c                   | working |
| `show_thinking` toggle                                | settings + `transcript_present/*` | P4.b                        | working |
| `input_prediction` setting                            | settings + `predict.lua`          | P2.a                        | working |
| `restrict_to_workspace` setting                       | settings + permissions            | P5                          | working |
| `redact_secrets` setting                              | settings + `engine/redact.rs`     | n/a                         | working |
| `auto_compact` setting                                | settings + compact loop           | P2                          | working |
| `multi_agent` setting                                 | settings + agent gating           | P5.c — replaced by Lua-side toggle in `plugins/multi_agent.lua`; engine has no multi-agent concept | working |
| `context_window` override                             | settings                          | n/a                         | working |
| Custom statusline items (`smelt.statusline.register`) | `lua/api/dispatch.rs`             | P4.c (cells-driven)         | working |
| Vim mode opt-in                                       | settings + `plugins/toggles.lua`  | P4                          | working |

## Persistence & lifecycle

| Feature                                             | Source today                                                       | Restored by | Status  |
| --------------------------------------------------- | ------------------------------------------------------------------ | ----------- | ------- |
| Auto-save every turn                                | `persist.rs` + `session.rs`                                        | P2.a        | working |
| Resume (`-r` / `/resume`)                           | `persist.rs` + `plugins/resume.lua`                                | P2.a + P4.d | working |
| Session branching / fork (`/fork`)                  | `plugins/session.lua` + `app/history.rs::fork_session` | P2.a + P4.e | working |
| Rewind to turn (`/rewind`, Esc Esc)                 | `app/history.rs` + `plugins/rewind.lua`                            | P2.a + P4.d | working |
| Conversation export (markdown → clip/file)          | `plugins/export.lua`                                               | P4.e        | working |
| Message queuing (queue while running, pop on Enter) | `app/events.rs` + `app/working.rs`                                 | P2          | working |
| Per-workspace permissions                           | `engine/permissions/workspace.rs` + `tui/workspace_permissions.rs` | P5.c        | working |
| Session-scoped permissions                          | `engine/permissions/approvals.rs`                                  | P5.c        | working |
| Last-model cache                                    | `state.rs` + cache                                                 | P2.a        | working |
| XDG dir support                                     | `engine/paths.rs`                                                  | n/a         | working |
| OAuth keyring                                       | `engine/auth.rs` + `provider/auth_storage.rs`                      | n/a         | working |
| Sleep inhibit during long turns                     | `sleep_inhibit.rs`                                                 | P2          | working |
| Terminal focus tracking (term_focused)              | `app/events.rs`                                                    | P2          | working |
| Graceful shutdown (Shutdown event)                  | `engine/agent.rs` + `app/events.rs`                                | P2.d        | working |

## Plugin / scripting surface

| API                                                     | Source today                        | Restored by                                | Status         |
| ------------------------------------------------------- | ----------------------------------- | ------------------------------------------ | -------------- |
| `~/.config/smelt/init.lua` autoload                     | `lua/mod.rs`                        | P3.b                                       | working        |
| `smelt.cmd.register`                                    | `cmd.lua` + `lua/api/dispatch.rs`   | P3.b → `lua/api/cmd.rs`                    | working        |
| `smelt.cmd.picker`                                      | `cmd.lua`                           | P3.b                                       | working        |
| `smelt.tools.register`                                  | `lua/tasks.rs` (PluginToolEnv)      | P3.b → `lua/api/tools.rs`                  | working        |
| `smelt.au.on` / `smelt.au.fire` namespace               | `lua/api/dispatch.rs::register_au` + `app/cells.rs` | P2.a.4b (landed; thin alias over `Cells::{subscribe_kind, set_dyn}`); P2.a.9 made it the only event-subscribe surface (the legacy `smelt.on` retired with the parallel autocmd registry) | working |
| Built-in event cells: `turn_start`, `turn_end`, `tool_start`, `tool_end`, `block_done`, `cmd_pre`, `cmd_post`, `session_started`, `session_ended`, `input_submit`, `shutdown`, `confirm_requested`, `confirm_resolved`, `turn_complete`, `turn_error`, `history`; stateful: `agent_mode`, `vim_mode`, `model`, `reasoning`, `confirms_pending`, `tokens_used`, `errors`, `cwd`, `session_title`, `branch`, `now`, `spinner_frame` | `app/cells.rs::build_with_builtins` | P2.a.9 (autocmd registry collapsed into cells; mode_change / model_change fold into `agent_mode` / `model` whose payload is the new value) | working |
| `smelt.keymap.set`                                      | `lua/api/dispatch.rs`               | P3.b                                       | working        |
| `smelt.keymap.help`                                     | `plugins/help.lua` reads            | P3.b                                       | working        |
| `smelt.spawn` (async task)                              | `_bootstrap.lua` + `lua/task.rs`    | P2                                         | working        |
| `smelt.sleep`                                           | `_bootstrap.lua`                    | P2                                         | working        |
| `smelt.task.wait` / `task.resume`                       | `_bootstrap.lua` + `lua/task.rs`    | P2                                         | working        |
| `smelt.tools.call` (call tool from tool)                | `_bootstrap.lua`                    | P5                                         | working        |
| `smelt.engine.ask`                                      | `lua/api/state.rs`                  | P3.b → `lua/api/engine.rs`                 | working        |
| `smelt.engine.model` / `models` / `set_model`           | `lua/api/state.rs`                  | P3.b                                       | working        |
| `smelt.engine.history`                                  | `lua/api/state.rs`                  | P3.b                                       | working        |
| `smelt.engine.cancel`                                   | `lua/api/state.rs`                  | P3.b                                       | working        |
| `smelt.ui.dialog.open` / `open_handle`                  | `dialog.lua` + `lua/api/widgets.rs` | P3.b → `lua/api/ui.rs`                     | working        |
| `smelt.ui.picker`                                       | `picker.lua` + `lua/api/widgets.rs` | P3.b                                       | working        |
| `smelt.ui.ghost_text`                                   | `lua/api/widgets.rs`                | P3.b                                       | working        |
| `smelt.session.*` (title/cwd/turns/rewind_to)           | `lua/api/state.rs`                  | P3.b → `lua/api/session.rs`                | working        |
| `smelt.settings.*`                                      | `lua/api/state.rs`                  | P3.b                                       | working        |
| `smelt.permissions.list/sync`                           | `lua/api/state.rs`                  | P3.b → `lua/api/permissions.rs`            | working        |
| `smelt.theme.snapshot/get/set/apply`                    | `lua/api/widgets.rs`                | P3.b → `lua/api/theme.rs` (registry)       | working        |
| `smelt.theme.use(name)`                                 | `_bootstrap.lua`                    | P4.a (sugar over `require("smelt.colorschemes." .. name)`) | working        |
| `smelt.theme.link(from, to)`                            | `lua/api/theme.rs`                  | P4.a (thin wrapper over `Theme::link`)     | working        |
| `smelt.clipboard.{read,write}` + legacy `__call` write   | `lua/api/clipboard.rs`                | P3.b ✓                                     | working |
| `smelt.process.*` (spawn/list/kill)                     | `lua/api/state.rs`                  | P3.b → `lua/api/process.rs`                | working        |
| `smelt.fuzzy.*`                                         | `_bootstrap.lua`                    | P3.b                                       | working        |
| `smelt.notify` / `smelt.notify_error`                   | `lua/api/mod.rs`                    | P3.b                                       | working        |
| `smelt.buf.*` (create/lines/text/extmark)               | `lua/api/widgets.rs`                | P3.b → `lua/api/buf.rs` (extmarks!)        | working        |
| `smelt.win.*`                                           | `lua/api/widgets.rs`                | P3.b → `lua/api/win.rs`                    | working        |
| `smelt.statusline.register/set`                         | `lua/api/dispatch.rs`               | P4.c (cells-driven spec)                   | working        |
| `smelt.cell.new/get/set/subscribe`                      | `lua/api/dispatch.rs` + `app/cells.rs` | P2.a.4b (landed; `smelt.cell(name)` handle + `:glob_subscribe` shipped); a.4c migrates built-ins | working |
| `smelt.defer(ms, fn)` (one-shot timer)                  | `lua/api/dispatch.rs`               | thin alias over `smelt.timer.set`          | working        |
| `smelt.timer.set/every/cancel` namespace                | `lua/api/dispatch.rs` + `app/timers.rs` | P2.a.5 (landed; cancellable handles)   | working        |
| `smelt.path` (`normalize / canonical / relative / expand / join / parent / basename / extension / is_absolute`) | `lua/api/path.rs` + `tui/path.rs` | P3.a + P3.c (landed `de7fb87`) | working |
| `smelt.fs` (`read / write / exists / is_file / is_dir / read_dir / mkdir{_all} / remove_* / rename / copy / mtime / size`) | `lua/api/fs.rs` + `tui/fs.rs` | P3.a + P3.c (landed this session) | working |
| `smelt.os` (`getenv / setenv / unsetenv / platform / arch / tempdir / home / cwd / set_cwd / pid`) | `lua/api/os.rs` | P3.c (landed this session) | working |
| `smelt.grep` (`run(pattern, path, opts)` over ripgrep — content / files_with_matches / count modes; case / multiline / context / glob / type / timeout) | `lua/api/grep.rs` + `tui/grep.rs` | P3.a + P3.c (landed this session) | working |
| `smelt.http` (`get(url, opts)` over `reqwest::blocking` — timeout / max_redirects / headers; returns `{ status, final_url, headers, body }`) | `lua/api/http.rs` + `tui/http.rs` | P3.a + P3.c (landed this session) | working |
| `smelt.html` (`title / links / to_text` over `scraper`) | `lua/api/html.rs` + `tui/html.rs` | P3.a + P3.c (landed this session) | working |
| `smelt.process.run` (`run(cmd, args, opts)` short-lived spawn over `tui::process` — cwd / env / stdin / timeout) | `lua/api/process.rs` + `tui/process.rs` | P3.a + P3.c (landed this session) | working |
| `smelt.notebook.parse` (Jupyter `.ipynb` parse over `tui::notebook`) | `lua/api/notebook.rs` + `tui/notebook.rs` | P3.a + P3.c (landed this session) | working |
| `smelt.parse` | _missing today_                     | P3.c                                       | offline-pre-P3 |

## Headless / non-TUI modes

| Feature                                         | Source today                           | Restored by            | Status  |
| ----------------------------------------------- | -------------------------------------- | ---------------------- | ------- |
| Headless run (`--headless`)                     | `app/headless.rs` + `src/main.rs`      | P2 (no-Ui coordinator) | working |
| Inline message arg (auto-submit)                | `src/main.rs::message: Option<String>` | n/a                    | working |
| Text output (final on stdout, tools on stderr)  | `app/headless.rs`                      | P2                     | working |
| JSON output (`--format json` JSONL events)      | `app/headless.rs`                      | P2                     | working |
| Verbose tool output (`-v`)                      | `src/main.rs`                          | n/a                    | working |
| Subagent mode (`--subagent`, → `--agent <id>` in target) | `src/main.rs` + `engine/socket.rs`     | P5.c (`tui::subprocess::socket`); flag rename in P5.e | working |
| Parent PID tracking (`--parent-pid`)            | `src/main.rs`                          | n/a                    | working |
| Subagent depth (`--depth`, `--max-agent-depth`) | `src/main.rs` + `engine/registry.rs`   | n/a                    | working |
| Concurrent agent cap (`--max-agents`)           | `engine/registry.rs`                   | n/a                    | working |
| Color control (`--color`)                       | `src/main.rs`                          | n/a                    | working |
| Log level (`--log-level`)                       | `src/main.rs` + `engine/log.rs`        | n/a                    | working |
| Bench mode (`--bench`)                          | `src/main.rs` + `metrics.rs`           | P7                     | working |

## CLI flags & configuration

| Group                   | Items                                                                                                                                      | Source                                    | Restored by | Status  |
| ----------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------- | ----------- | ------- |
| Connection              | `--config`, `-m/--model`, `--api-base`, `--api-key-env`, `--type`                                                                          | `src/main.rs`                             | n/a         | working |
| Behavior                | `--mode`, `--mode-cycle`, `--reasoning-effort`, `--reasoning-cycle`, `--no-tool-calling`, `--system-prompt`, `--no-system-prompt`, `--set` | `src/main.rs`                             | n/a         | working |
| Sampling                | `--temperature`, `--top-p`, `--top-k`                                                                                                      | `src/main.rs`                             | n/a         | working |
| Sessions                | `-r/--resume [SESSION_ID]`                                                                                                                 | `src/main.rs`                             | n/a         | working |
| Multi-Agent             | `--multi-agent`/`--no-multi-agent`, `--max-agent-depth`, `--max-agents`                                                                    | `src/main.rs`                             | n/a         | working |
| Runtime                 | `--headless`, `--format`, `-v`, `--color`, `--log-level`, `--bench`                                                                        | `src/main.rs`                             | n/a         | working |
| Subcommands             | `smelt auth`                                                                                                                               | `src/main.rs`                             | n/a         | working |
| Config: providers       | `name`, `type`, `api_base`, `api_key_env`, `models`                                                                                        | `engine/config_file.rs`                   | n/a         | working |
| Config: defaults        | `model`, `mode`, `mode_cycle`, `reasoning_effort`, `reasoning_cycle`                                                                       | config                                    | n/a         | working |
| Config: auxiliary       | `model`, `use_for: { title, ... }`                                                                                                         | config                                    | n/a         | working |
| Config: settings        | (see Theming + Persistence sections above)                                                                                                 | `state.rs`                                | P2.a        | working |
| Config: theme           | `accent` (preset or 0-255)                                                                                                                 | config + `theme.rs`                       | P1.0        | working |
| Config: mcp             | `command`, `type`, `env`, `timeout`, `enabled`                                                                                             | `engine/mcp/`                             | n/a         | working |
| Config: skills          | `paths`                                                                                                                                    | `engine/skills.rs`                        | n/a         | working |
| Config: permissions     | per-tool/per-mode allow/ask/deny                                                                                                           | `engine/permissions/`                     | P5.c        | working |
| Config: model sampling overrides | `temperature`, `top_p`, `top_k`, `min_p`, `repeat_penalty` | `protocol/usage.rs::ModelConfigOverrides` | n/a | working |
| Config: model `tool_calling` flag | per-model toggle for native tool-calling | `protocol/usage.rs::ModelConfig` (not in `ModelConfigOverrides`) | n/a | working |
| Config: model `pricing` overrides | per-model input/output token costs | `engine/pricing.rs::ModelPricing` (resolved at config load) | n/a | working |

---

## Verification log

After each phase, write a short note here. Pre-P0 audits (2026-04-28, two passes, parallel agents) blessed the matrix as the canonical surface — counts and source pointers verified against the live tree.
