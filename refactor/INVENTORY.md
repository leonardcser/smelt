# Inventory — current codebase state and target fate

Every source file in the repo, mapped against the refactor plan. Updated as
phases land. **This is the master list — when you're about to delete or move
something, check here first.**

Maintenance rules:

- When a phase lands, update the **Status** column for every row it touched
  (mark `done`, note the actual destination).
- When a fate decision changes mid-phase, update the row immediately and log the
  decision in the current `P<n>.md`.
- A new file added to the codebase = a new row here, in the same commit.
- "Unclear" rows demand a decision before the relevant phase begins.

Legend for **Fate**:

- `kept` — survives roughly as-is (may have internal edits).
- `renamed` — same logic, new name/path.
- `merged` — folds into another file/type.
- `restructured` — splits responsibilities across multiple new locations.
- `deleted` — gone entirely.
- `moved-to-lua` — responsibility moves to `runtime/lua/smelt/`.
- `moved-to-capability` — promoted to `tui::<name>` capability module.
- `unclear` — fate not yet decided; needs explicit decision before phase.

Legend for **Status**: `pending` (not yet touched), `in-progress`, `done`.

---

## `crates/ui/src/`

| File                  | LOC  | Purpose                                       | Fate         | Phase     | Status  | Notes                                                                                               |
| --------------------- | ---- | --------------------------------------------- | ------------ | --------- | ------- | --------------------------------------------------------------------------------------------------- |
| `buffer.rs`           | 1473 | Lines, namespaced extmarks, marks, virtual text | restructured | P1.a    | partial | `attach(spec)` parser-hook system + `BufferFormatter` deletion deferred to P1.a-tail (gated on transcript-pipeline migration). |
| `callback.rs`         | 367  | Window callbacks + WinEvent routing           | kept         | P2        | pending | Drain pipeline for Lua callbacks; pattern preserved. |
| `clipboard.rs`        | 81   | Clipboard subsystem (kill ring + sink)        | restructured | P1.d.5b   | partial | App-level subsystem; every text I/O routes through `app.clipboard.{read,write}`. |
| `compositor.rs`       | 175  | Double-buffered renderer (grid + diff flush)  | restructured | P1.f      | partial | Renderer-only (`render_with` + grid diff). Final fold into `Ui` facade is P1.f. |
| `dialog.rs`           | 87   | Slim panel-spec types (`PanelSpec` / `PanelHeight`) consumed by the overlay translator | restructured | P1.c | partial | Pure data types remain; full deletion (move into `tui::lua::ui_ops`) deferred to P1.f.                                       |
| `edit_buffer.rs`      | 323  | EditBuffer trait abstraction                  | merged       | P1.a      | pending | Rolled into Buffer (per-buffer edit history)                                                        |
| `event.rs`            | 107  | `Event` / `Status` / `FocusTarget` dispatch surface | added | P2.b.3 | landed | `Event` wraps crossterm payloads; `Status` replaces `DispatchOutcome` + `MouseAction`; `FocusTarget` is the semantic keyboard-focus alias. |
| `flush.rs`            | 182  | Diff → SGR writer                             | kept         | P1        | pending | Grid diff persists as primitive                                                                     |
| `grid.rs`             | 419  | Terminal frame (cells + style)                | kept         | P1        | pending | Primitive                                                                                           |
| `id.rs`               | 39   | `BufId`, `WinId` newtypes                     | kept         | P1        | pending | Add `OverlayId`                                                                                     |
| `kill_ring.rs`        | 241  | Kill ring                                     | kept         | P1.d.5b   | partial | Data container; storage lives on `ui::Clipboard`. |
| `layout.rs`           | 1098 | `LayoutTree` Vbox/Hbox/Leaf(WinId) + Constraint(7) + Chrome + Anchor(5) + natural_size + paint_chrome | restructured | P1.b/P1.c | partial | Target shape landed. |
| `lib.rs`              | 2619 | `Ui` facade + `UiHost` trait                  | restructured | P1.f/P2.b.2/P2.b.3/P2.b.4c.3/P2.b.4c.5a | partial | Splits / overlays / focus / hit_test / capture / cursor_shape API operational; `dispatch_event(ui::Event) -> ui::Status` consumes the ui-owned terminal-event enum and now owns the scrollbar drag gesture; `Ui::resolve_split_mouse(MouseEvent) -> Option<(WinId, u8)>` latches `HitTarget::Window` capture on Down + records click count, returns the captured win on Drag/Up + clears capture on Up; `hit_test` returns `HitTarget::Scrollbar` for splits leaves with painted bars; `UiHost` trait + `Ui` impl shipped in P2.b.2 (no `Host` supertrait). Full facade rewrite still ahead (F.6 tail). |
| `overlay.rs`          | 455  | `Overlay` + `OverlayId` + `OverlayHitTarget` + `HitTarget` + `Anchor` resolver | added        | P1.c      | partial | Types + resolution + hit-test operational; legacy float surface gone (overlays + splits are the only containers). |
| `style.rs`            | 18   | Style helpers                                 | kept         | P1        | pending | Theme registry references hl ids; raw style stays                                                   |
| `text.rs`             | 242  | Text utilities (word boundaries)              | kept         | P1        | pending | Utility                                                                                             |
| `theme.rs`            | 204  | Highlight group registry (nvim-style)         | kept         | P1.0      | landed  | Replaces host `crate::theme::*` flat module; plumbed through DrawContext.                             |
| `undo.rs`             | 66   | Undo/redo snapshots                           | merged       | P1.a      | pending | Into Buffer per-buffer edit history                                                                 |
| `vim.rs`              | 3043 | Vim state machine                             | deleted      | P1.d.5    | partial | 5d (registers/dot-repeat/undo → Buffer) + 5f.2d (recipe-style flatten, gated on Lua keymap registry) still pending. |
| `motions.rs`          | 333  | Cursor-motion primitives over `&str` (h/j/k/l/w/b/e/f/F/t/T/%, FindKind) | renamed      | P1.d.5f.2b | landed  | Top-level primitive. |
| `text_objects.rs`     | 157  | Vim-shaped text-object selection (iw/aw/i"/a"/i(/a(, etc.) | renamed      | P1.d.5f.2b | landed  | Top-level primitive. |
| `window.rs`           | 1163 | Window viewport                               | restructured | P1.d/P2.b.3/P2.b.4a | partial | `render(buf, slice, ctx)` + cursor + scrollbar paint operational; `handle_key` and `handle_mouse` both return `ui::Status` (`Option<Option<String>>` retired; kill-ring inspection now lives at the caller); per-Window vim state inline (`vim_state`, `selection_anchor`, `curswant`); virt-text paints after highlights. Remaining decomposition waits for P2.b.4b unified `Window::handle` entry + P1.d.5d/f.2d. |

## `crates/tui/src/`

| File                                      | LOC  | Purpose                                                     | Fate                | Phase     | Status  | Notes                                                                                                       |
| ----------------------------------------- | ---- | ----------------------------------------------------------- | ------------------- | --------- | ------- | ----------------------------------------------------------------------------------------------------------- |
| `alloc.rs`                                | 66   | Allocation tracking                                         | kept                | none      | pending | Utility                                                                                                     |
| `api.rs`                                  | 123  | Public crate API surface                                    | kept                | P3        | pending | May fold into Lua bindings                                                                                  |
| `app/mod.rs`                              | 1574 | `TuiApp` god-struct + event loop                            | restructured        | P2.a/P2.b | partial | `TuiApp { core: Core, well_known: WellKnown, ui: ui::Ui, … }`; headless / subagent flows live on `HeadlessApp` now. |
| `app/agent.rs`                            | 1453 | Multi-agent lifecycle, message relay                        | restructured        | P5.b/P5.c | partial | Lua-side multi-agent registry migration stays P5.b/P5.c. |
| `app/app_config.rs`                       | 33   | `AppConfig` data carve-out (provider triple, mode + reasoning cycles, settings, multi-agent toggle, model/cli overrides, context window) | kept                | P2.a.6    | landed  | Stays as a Core subsystem field after the P2.a.12 split. |
| `app/cells.rs`                            | 960  | `Cells` reactive primitive (typed name → value + per-`TypeId` Lua projector + glob subs + queue-then-drain + `build_with_builtins`) | kept                | P2.a.4    | landed  | All built-in cells declared and projector-backed; `turn_complete` / `turn_error` publish call sites relocate alongside their handlers when the engine-event drain folds into `EngineBridge` (P2.d). |
| `app/cmdline.rs`                          | 431  | Cmdline buffer + key dispatcher (overlay path)              | moved-to-lua        | P4        | partial | Buffer-backed input leaf in a `ScreenBottom` modal overlay; P4 moves recipe to `widgets/cmdline.lua`. |
| `app/commands.rs`                         | 529  | `RUST_COMMANDS` table                                       | moved-to-lua        | P4.e      | partial | Lua command migration is P4.e. |
| `app/confirms.rs`                         | 71   | `Confirms` subsystem (pending tool-approval requests)       | kept                | P2.a.3    | landed  | `is_clear()` is the engine-drain gate (consumed by EngineBridge in a.11). |
| `app/core.rs`                             | 83   | `Core` aggregate of headless-safe subsystems (`config` / `session` / `confirms` / `clipboard` / `timers` / `cells` / `lua` / `engine`); reached as `TuiApp.core.<field>` / `HeadlessApp.core.<field>` | kept                | P2.a.12a  | landed  | `Core::new(config, engine)` constructed by both `TuiApp::new` (TUI) and `HeadlessApp::new`; `tools: ToolRuntime` slot stays vacant pending P5.a's trait shape. |
| `app/engine_bridge.rs`                    | 42   | `EngineBridge` carve-out over `EngineHandle` (send / recv / try_recv / processes / drain_spawned) | kept | P2.a.11 | landed  | Stays as a Core subsystem field (`engine_bridge`) after the P2.a.12 split; P2.d folds the engine-event drain into this type. |
| `app/content_keys.rs`                     | 238  | Content pane keymap                                         | restructured        | P4        | partial | Becomes a Window keymap recipe in P4. |
| `app/dialogs/confirm.rs`                  | 149  | Confirm dialog (title + options helpers)                    | moved-to-lua        | P4.d      | partial | Hosts `render_title_into_buf` + `build_options` only. Migration to `dialogs/confirm.lua` stays P4.d. |
| `app/dialogs/confirm_preview.rs`          | 298  | Diff preview pane                                           | moved-to-lua        | P4        | pending | `diff.lua` extmark presentation                                                                             |
| `app/dialogs/mod.rs`                      | 7    | Module exports                                              | deleted             | P4.d      | pending | Empty after migrations                                                                                      |
| `app/dialogs/text_modal.rs`               | 70   | Read-only text modal (`/stats`, `/cost`)                    | moved-to-lua        | P4        | partial | Rewritten as overlay with parity-restored Esc / q / Ctrl+C dismiss. P4 moves it to Lua.                  |
| `app/events.rs`                           | 861  | Event dispatch (key/mouse/engine)                           | restructured        | P2.b/P2.e | partial | Single `select!` loop fold still ahead (P2.e). |
| `app/headless.rs`                         | 228  | `HeadlessSink` (output writer for headless / subagent: format + verbose state, JSON / text / log helpers) | kept                | P2        | partial | Stays a thin sink wrapper around `HeadlessApp`'s output surface. |
| `app/headless_app.rs`                     | 528  | `HeadlessApp { core, sink, next_turn_id }` frontend: one-shot `run_oneshot` + persistent `run_subagent` | kept                | P2.a.12b1 | landed  | First non-TUI frontend over `Core`. |
| `app/history.rs`                          | 689  | Rewind/fork/compact state                                   | merged              | P2        | partial | Reads/writes route through `self.session.*`. Final merge into Session subsystem stays P2.a.8+. |
| `app/host.rs`                             | 173  | `Host` + `UiHost` trait impls. `Host` is the Ui-agnostic accessor surface (`clipboard / cells / timers / lua / engine / session / confirms`) impl'd for `Core / TuiApp / HeadlessApp`. `UiHost` (the trait itself lives in `crates/ui/src/lib.rs`) is the compositor-bearing surface impl'd for `TuiApp` only. | kept                | P2.b.1/P2.b.2 | landed  | `tools()` waits on a.10. `Window::handle` collapse + Lua TLS split ride later P2.b sub-phases. |
| `app/lua_bridge.rs`                       | 152  | Lua FFI dispatch glue                                       | restructured        | P3        | pending | Per-namespace binding files                                                                                 |
| `app/lua_handlers.rs`                     | 131  | Lua async handlers                                          | kept                | P2/P3     | partial | Reads through `self.clipboard` + `self.session.messages`. |
| `app/mouse.rs`                            | 691  | Mouse routing + drag + scrollbar                            | restructured        | P1/P2     | partial | Viewport reads via `Ui::win(id)?.viewport`; click-count via `Ui::record_click`; scrollbar gesture absorbed Ui-side (`propagate_scrollbar_scroll` mirrors the new `scroll_top`); wheel via `Ui::hit_test`; Down/Drag/Up Left for splits-leaf Windows fold onto `Ui::resolve_split_mouse` + `HitTarget::Window` capture (`dispatch_focused_mouse` / `extend_selection_to` retire); per-pane data through `UiHost` + drag autoscroll fold across P2.b.4c.5b/5c/6. |
| `app/pane_focus.rs`                       | 114  | Prompt vs Content focus cycling                             | restructured        | P2        | pending | Replaced by Ui::focus_next/focus_prev                                                                       |
| `app/render_loop.rs`                      | 447  | Per-frame render                                            | kept                | P2.e      | partial | Drives painted splits + cursor_shape per frame. Single `select!` loop hookup is P2.e. |
| `app/status_bar.rs`                       | 296  | Statusline rendering                                        | moved-to-lua        | P4.c      | partial | Writes into the status `Buffer`; Cells-driven spec migration to Lua is P4.c. |
| `app/timers.rs`                           | 176  | `Timers` subsystem (scheduled Lua callbacks: one-shot + recurring) | kept                | P2.a.5    | landed  | Stays as a Core subsystem field after the P2.a.12 split. |
| `app/transcript.rs`                       | 871  | Transcript line/block management                            | restructured        | P2/P4     | partial | `transcript.lua` migration is P4.b. |
| `app/transcript_cache.rs`                 | 224  | Block layout cache                                          | deleted             | P4.b      | pending | Extmarks replace IR cache                                                                                   |
| `app/transcript_model.rs`                 | 968  | Block / ViewState / ToolState                               | restructured        | P2/P4     | pending | Splits into Session.history + on_block extmark population                                                   |
| `app/transcript_present/mod.rs`           | 1153 | Transcript render orchestrator                              | moved-to-lua        | P4.b      | pending | `transcript.lua` controller                                                                                 |
| `app/transcript_present/agent.rs`         | 118  | Agent message presentation                                  | deleted             | P5.c      | pending | Sub-agent replies are tool calls; transcript renders them as ordinary tool blocks. No dedicated widget.     |
| `app/transcript_present/markdown.rs`      | 351  | Markdown presentation                                       | moved-to-lua        | P4.b      | pending | `transcript.lua` on_block + `tui::parse`                                                                    |
| `app/transcript_present/tools.rs`         | 477  | Tool result formatting                                      | moved-to-lua        | P4.b      | pending | Lua extmarks                                                                                                |
| `app/transcript_present/tool_previews.rs` | 146  | Tool preview panels                                         | moved-to-lua        | P4.b      | pending | Lua extmarks                                                                                                |
| `app/working.rs`                          | 366  | Spinner + phase tracking                                    | merged              | P2.c/P2.d | pending | `spinner_frame` cell + Timers + EngineBridge fires                                                          |
| `attachment.rs`                           | 237  | Prompt attachments                                          | restructured        | P1/P4     | pending | Extmarks in `attachments` namespace + Lua input widget                                                      |
| `builtin_commands.rs`                     | 121  | Builtin slash commands                                      | moved-to-lua        | P4.e      | pending | `commands.lua`                                                                                              |
| `completer/command.rs`                    | 113  | Command completion                                          | restructured        | P4        | pending | Lua input widget helper                                                                                     |
| `completer/file.rs`                       | 151  | File path completion                                        | restructured        | P4        | pending | Lua input widget helper                                                                                     |
| `completer/history.rs`                    | 158  | History completion                                          | restructured        | P4        | pending | Lua input widget helper                                                                                     |
| `completer/mod.rs`                        | 192  | Completer state machine                                     | restructured        | P1.d/P4   | pending | Decomposes: ghost-text extmark + Overlay picker + prompt keymap recipe                                      |
| `completer/score.rs`                      | 62   | Fuzzy scorer                                                | moved-to-capability | P3        | pending | Into `tui::fuzzy`                                                                                           |
| `config.rs`                               | 636  | Config loader                                               | merged              | P2.a      | partial | Loader + `SettingsConfig` / `AuxiliaryRouting` types stay here pending the full settings merge into `AppConfig`. |
| `content/context.rs`                      | 24   | Content context struct                                      | merged              | P1        | pending | Into Window render path                                                                                     |
| `content/display.rs`                      | 210  | Display rendering utility                                   | restructured        | P1        | pending | Window render pipeline                                                                                      |
| `content/highlight/diff.rs`               | 497  | Diff highlight logic                                        | restructured        | P3/P4     | pending | LCS to `tui::parse`; presentation to `diff.lua`                                                             |
| `content/highlight/inline.rs`             | 1134 | Markdown / inline code parsing                              | restructured        | P3/P4     | pending | Parse to `tui::parse`; extmark population in Lua                                                            |
| `content/highlight/mod.rs`                | 48   | Highlight module exports                                    | deleted             | P4        | pending | Replaced by Buffer.attach + extmarks                                                                        |
| `content/highlight/syntax.rs`             | 303  | syntect adapter                                             | moved-to-capability | P3        | pending | Into `tui::parse`                                                                                           |
| `content/highlight/util.rs`               | 196  | Highlight utilities                                         | restructured        | P3/P4     | pending | Some to `tui::parse`, some to Lua                                                                           |
| `content/layout.rs`                       | 198  | Content layout (transcript + prompt + status LayoutTree)    | restructured        | P1.b/P1.f.3 | landed  | `build_layout_tree(input, status_win)` + `LayoutState::from_ui`; status is a `Length(1)` leaf in an inner vbox. |
| `content/layout_out.rs`                   | 281  | Layout output                                               | restructured        | P1.b      | pending | Into Ui render path                                                                                         |
| `content/mod.rs`                          | 144  | Module exports                                              | deleted             | P4        | pending | Most submodules go away                                                                                     |
| `content/prompt_data.rs`                  | 971  | Prompt content (formatting, completion)                     | restructured        | P2/P4     | partial | `compute_prompt` populates the prompt buffer + drives `wins[PROMPT_WIN]` cursor / viewport / completer virt-text. Final widget-recipe migration is P4. |
| `content/prompt_wrap.rs`                  | 233  | Prompt wrap state                                           | deleted             | P1.a      | pending | Wrap state lives on Buffer                                                                                  |
| `content/selection.rs`                    | 518  | Selection tracking                                          | restructured        | P1/P2     | pending | Window selection + theme.get("Visual")                                                                      |
| `content/status.rs`                       | 208  | Statusline content model                                    | moved-to-lua        | P4.c      | partial | Spec segments + cell bindings migration to Lua is P4.c. |
| `content/stream_parser.rs`                | 785  | Streaming markdown/diff parser                              | restructured        | P3        | pending | Into `tui::parse`, called from Buffer.attach                                                                |
| `content/to_buffer.rs`                    | 248  | Content → terminal buffer                                   | restructured        | P1        | pending | Window render to Grid                                                                                       |
| `content/transcript.rs`                   | 1115 | Transcript display                                          | moved-to-lua        | P4.b      | pending | `transcript.lua`                                                                                            |
| `content/transcript_buf.rs`               | 75   | Transcript Buffer projection cache                          | deleted             | P4.b      | partial | Borrows the transcript display buffer; exports `NS_SELECTION`. Full deletion rides with `transcript.lua` migration in P4.b. |
| `content/viewport.rs`                     | 265  | Viewport scroll state                                       | merged              | P1.d      | pending | Into Window scroll state                                                                                    |
| `custom_commands.rs`                      | 322  | User-defined slash commands                                 | moved-to-lua        | P4.e      | pending | Plugin commands.lua                                                                                         |
| `format.rs`                               | 276  | Text formatting helpers                                     | kept                | none      | pending | Utility                                                                                                     |
| `fuzzy.rs`                                | 168  | Fuzzy matching                                              | moved-to-capability | P3        | pending | `tui::fuzzy` capability or stays here                                                                       |
| `input/buffer.rs`                         | 403  | Prompt edit buffer                                          | restructured        | P1.a/P4   | partial | Final merge into Buffer stays P1.a-tail. |
| `input/completer_bridge.rs`               | 281  | Completer integration                                       | restructured        | P1.d/P4   | pending | Ghost-text extmark + picker Overlay                                                                         |
| `input/history.rs`                        | 88   | Input history                                               | merged              | P2        | pending | Into Session.history                                                                                        |
| `input/mod.rs`                            | 2077 | Input subsystem (god-struct)                                | restructured        | P1/P2/P4  | partial | Carve into prompt Window + completer + attachments stays P1/P2/P4. |
| `input/vim_bridge.rs`                     | 101  | Vim mode in input                                           | restructured        | P2        | partial | Window keymap recipe still pending P2. |
| `instructions.rs`                         | 61   | System instructions model                                   | kept                | P2        | pending | Config data                                                                                                 |
| `keymap.rs`                               | 860  | Global keymap registry                                      | restructured        | P3/P4     | pending | Recipes are Lua; registry holds (mode, key) → recipe id                                                     |
| `lib.rs`                                  | 75   | Crate root                                                  | kept                | none      | pending | Re-exports update as modules move                                                                           |
| `lua/api/mod.rs`                          | 293  | Lua API module organization                                 | restructured        | P3.b      | partial | Per-namespace P3.b split still pending. |
| `lua/api/dispatch.rs`                     | 678  | Lua FFI dispatch                                            | restructured        | P3.b      | partial | Hosts `smelt.timer.*` + `smelt.cell.*` + `smelt.au.*` registrations (the `smelt.on` autocmd binding retired with the parallel registry in P2.a.9); per-namespace P3.b split still pending. |
| `lua/api/state.rs`                        | 677  | Lua state-getter bindings                                   | restructured        | P3.b      | partial | Per-namespace split stays P3.b. |
| `lua/api/widgets.rs`                      | 633  | Lua widget creation API                                     | restructured        | P3.b      | partial | P3.b splits into ui.rs / win.rs / buf.rs. |
| `lua/app_ref.rs`                          | 122  | TLS app pointer                                             | kept                | P2.b.5    | partial | `with_host` / `try_with_host` (`pub(crate)`) + `with_ui_host` / `try_with_ui_host` (`pub`) trait-typed dispatchers shipped in P2.b.5a; `with_app` retained pending b.5c bulk migration. TLS-slot generalization (Tui-or-Headless) waits for b.5b alongside the first headless Lua driver. |
| `lua/confirm_ops.rs`                      | 151  | Lua confirm bindings                                        | moved-to-lua        | P4.d      | partial | `_get` dropped (request payload reads from `confirm_requested` cell); `_render_title` / `_back_tab` / `_resolve` remain. Migration to `dialogs/confirm.lua` stays P4.d. |
| `lua/mod.rs`                              | 1545 | Lua runtime                                                 | restructured        | P2.a/P3   | partial | Autocmd registry retired (events flow through Cells now); final LuaRuntime carve into its own subsystem stays pending. |
| `lua/render_ops.rs`                       | 170  | Lua render helpers (smelt.diff/syntax/bash/notebook.render) | restructured        | P3.b      | pending | Splits into per-capability binding files                                                                    |
| `lua/task.rs`                             | 471  | Single coroutine task wrapper                               | kept                | P2        | pending | Coroutine task system                                                                                       |
| `lua/tasks.rs`                            | 576  | Coroutine task registry + plugin tool env                   | restructured        | P2/P5     | pending | Becomes ToolRuntime impl after P5                                                                           |
| `lua/ui_ops.rs`                           | 893  | Lua UI ops (overlay open/close)                             | restructured        | P3.b      | partial | Every Lua dialog routes through `open_dialog_via_overlay`; `OverlayPlacement` is the only placement vocabulary. P3.b splits into ui.rs / win.rs. |
| `metrics.rs`                              | 571  | Frame timing                                                | kept                | P7        | pending | Review for dead code at end                                                                                 |
| `perf.rs`                                 | 286  | Profiling guards                                            | kept                | P7        | pending | Review at end                                                                                               |
| `persist.rs`                              | 153  | Session save/load                                           | merged              | P2        | pending | Into Session subsystem                                                                                      |
| `picker.rs`                               | 314  | Buffer-backed picker overlay (open / set_items / set_selected) | restructured     | P1.c/P4  | partial | P4 moves the recipe behind `widgets/picker.lua`. |
| `prompt_sections.rs`                      | 206  | System prompt composition                                   | kept                | P2        | pending | Engine glue                                                                                                 |
| `session.rs`                              | 643  | Session struct (metadata, costs)                            | merged              | P2.a      | partial | SoT for messages + snapshots + running cost. Final merge into Session subsystem stays P2.a.8+. |
| `sleep_inhibit.rs`                        | 140  | OS sleep suppression                                        | kept                | P2        | pending | Subsystem or utility                                                                                        |
| `state.rs`                                | 340  | App state enums + ResolvedSettings                          | restructured        | P2.a      | partial | Saved-state `State` loader + persisted-settings types stay until the AppConfig persistence rewrite. |
| `theme.rs`                                | 368  | Theme bridge: populate_ui_theme + OSC11 detect + PRESETS    | narrowed            | P1.0      | landed  | Bridge fns + presets only; atomic globals collapsed onto `ui::Theme`. |
| `utils.rs`                                | 87   | General utilities                                           | kept                | none      | pending | Utility                                                                                                     |
| `window.rs`                               | 35   | Window wrapper (gutters, follow_tail)                       | merged              | P1.d      | pending | Into ui::Window                                                                                             |
| `workspace_permissions.rs`                | 202  | Workspace approval rules                                    | moved-to-capability | P3.a      | pending | Folds into `tui::permissions::store` (workspace JSON store); Lua tool hooks call via FFI                    |

## `crates/engine/src/`

| File                           | LOC  | Purpose                        | Fate         | Phase | Status  | Notes                                                           |
| ------------------------------ | ---- | ------------------------------ | ------------ | ----- | ------- | --------------------------------------------------------------- |
| `agent.rs`                     | 2129 | Multi-agent loop, turn driver  | restructured | P5    | pending | Drops `permissions: Permissions` field + `decide()` call; drops the multi-agent loop branch (~400 LOC); calls `ToolDispatcher::evaluate_hooks` only. Ends as single-agent only. |
| `auth.rs`                      | 133  | Auth config + token storage    | kept         | none  | pending | Provider infra                                                  |
| `cancel.rs`                    | 53   | Cancellation token             | kept         | P6    | pending | Used by cooperative cancel                                      |
| `compact.rs`                   | 553  | Context compaction             | kept         | none  | pending | LLM-side context reduction                                      |
| `config.rs`                    | 27   | Top-level engine config        | kept         | none  | pending |                                                                 |
| `config_file.rs`               | 122  | Config TOML loader             | kept         | none  | pending |                                                                 |
| `image.rs`                     | 105  | Image embedding                | kept         | none  | pending | Provider feature                                                |
| `lib.rs`                       | 440  | Engine exports + EngineHandle  | restructured | P5.a/P5.c  | pending | Tool trait slimmed; ToolDispatcher introduced; drop `EngineConfig.interactive`, `permissions`, `runtime_approvals` fields; `EngineHandle` becomes channels-only |
| `log.rs`                       | 120  | Trace logging                  | kept         | none  | pending |                                                                 |
| `mcp/mod.rs`                   | 286  | MCP manager                    | kept         | none  | pending | Multi-agent capability                                          |
| `mcp/tool_adapter.rs`          | 71   | MCP tool wrapper               | kept         | P5    | pending | Schema layer survives                                           |
| `paths.rs`                     | 151  | Workspace/config dir discovery | kept         | none  | pending |                                                                 |
| `permissions/approvals.rs`     | 228  | Runtime approval tracking      | moved-to-capability | P5.c  | pending | → `tui::permissions::approvals` (RuntimeApprovals queried by Lua hooks) |
| `permissions/bash.rs`          | 515  | Bash subcommand parsing        | moved-to-capability | P5.c  | pending | → `tui::permissions::bash` (FFI for `bash.lua` hook)            |
| `permissions/mod.rs`           | 298  | Permissions aggregate + decide | deleted             | P5.c  | pending | `Permissions` aggregate + per-mode `decide()` go away; Lua hooks compose pieces |
| `permissions/rules.rs`         | 308  | Rule matching                  | moved-to-capability | P5.c  | pending | → `tui::permissions::rules` (glob compile + ruleset check)      |
| `permissions/tests.rs`         | 1617 | Permission unit tests          | moved-to-capability | P5.c  | pending | Move with the code to `tui::permissions::tests`                 |
| `permissions/workspace.rs`     | 98   | Path extraction + boundary     | moved-to-capability | P5.c  | pending | → `tui::permissions::workspace` (path extract + workspace check) |
| `pricing.rs`                   | 204  | Token cost calc                | kept         | none  | pending | Session/budget                                                  |
| `provider/anthropic.rs`        | 291  | Anthropic API                  | kept         | none  | pending |                                                                 |
| `provider/auth_storage.rs`     | 76   | Credential storage             | kept         | none  | pending |                                                                 |
| `provider/chat_completions.rs` | 215  | Streaming abstraction          | kept         | none  | pending |                                                                 |
| `provider/codex.rs`            | 701  | Codex provider                 | kept         | none  | pending |                                                                 |
| `provider/copilot.rs`          | 654  | Copilot provider               | kept         | none  | pending |                                                                 |
| `provider/extract.rs`          | 328  | JSON extraction                | kept         | none  | pending |                                                                 |
| `provider/mod.rs`              | 1135 | Provider trait + dispatch      | kept         | none  | pending | Core LLM boundary                                               |
| `provider/openai.rs`           | 351  | OpenAI provider                | kept         | none  | pending |                                                                 |
| `provider/sse.rs`              | 48   | SSE parsing                    | kept         | none  | pending |                                                                 |
| `redact.rs`                    | 922  | Content redaction              | kept         | none  | pending | Privacy/logging                                                 |
| `registry.rs`                  | 262  | Multi-agent on-disk registry (`RegistryEntry`, agent PIDs/sockets) | moved-to-capability | P5.c  | pending | → `tui::subprocess::registry` (the agent-tracking JSON file Lua tools maintain) |
| `skills.rs`                    | 213  | Skill loader                   | kept         | P5    | pending | `load_skill` tool wraps this                                    |
| `socket.rs`                    | 345  | Inter-agent IPC sockets        | moved-to-capability | P3.a/P5.c | pending | → `tui::subprocess::socket` (wire layer for sub-smelt subprocess type)         |
| `tools/background.rs`          | 228  | Background process registry    | moved-to-capability | P3.a  | pending | → `tui::process` (registry + spawn/group/kill); Lua bash tool registers with it |
| `tools/bash.rs`                | 355  | Bash tool                      | moved-to-lua | P5.b  | pending | `tools/bash.lua` composes `tui::process`                        |
| `tools/edit_file.rs`           | 233  | Edit tool                      | moved-to-lua | P5.b  | pending | `tools/edit_file.lua` composes `tui::fs`                        |
| `tools/file_state.rs`          | 340  | File metadata tracking         | moved-to-capability | P3.a  | pending | → `tui::fs::file_state` (mtime tracking for edit_file race detection)            |
| `tools/glob.rs`                | 101  | Glob tool                      | moved-to-lua | P5.b  | pending | `tools/glob.lua`                                                |
| `tools/grep.rs`                | 262  | Grep tool                      | moved-to-lua | P5.b  | pending | `tools/grep.lua` over `tui::grep`                               |
| `tools/list_agents.rs`         | 90   | List agents tool               | moved-to-lua | P5.b  | pending | `tools/list_agents.lua`                                         |
| `tools/load_skill.rs`          | 53   | Load skill tool                | moved-to-lua | P5.b  | pending | `tools/load_skill.lua`                                          |
| `tools/message_agent.rs`       | 98   | Message agent tool             | moved-to-lua | P5.b  | pending | `tools/message_agent.lua`                                       |
| `tools/mod.rs`                 | 567  | Tool trait + ToolResult + ctx  | restructured | P5.a  | pending | Trait slims (loses needs_confirm/preflight/approval_patterns)   |
| `tools/notebook.rs`            | 677  | Notebook tool                  | moved-to-lua | P5.b  | pending | `tools/notebook_edit.lua` over `tui::notebook`                  |
| `tools/peek_agent.rs`          | 66   | Peek agent tool                | moved-to-lua | P5.b  | pending | `tools/peek_agent.lua`                                          |
| `tools/read_file.rs`           | 320  | Read file tool                 | moved-to-lua | P5.b  | pending | `tools/read_file.lua`                                           |
| `tools/result_dedup.rs`        | 169  | Streaming result dedup         | moved-to-capability | P3.a  | pending | → `tui::tools::dedup` helper (or Lua-only if usage is small)                     |
| `tools/spawn_agent.rs`         | 232  | Spawn agent tool               | moved-to-lua | P5.b  | pending | `tools/spawn_agent.lua`                                         |
| `tools/stop_agent.rs`          | 52   | Stop agent tool                | moved-to-lua | P5.b  | pending | `tools/stop_agent.lua`                                          |
| `tools/web_cache.rs`           | 51   | HTTP cache                     | moved-to-capability | P3.a  | pending | → `tui::http::cache`                                                             |
| `tools/web_fetch.rs`           | 268  | Web fetch tool                 | moved-to-lua | P5.b  | pending | `tools/web_fetch.lua` over `tui::http`+`tui::html`              |
| `tools/web_search.rs`          | 194  | Web search tool                | moved-to-lua | P5.b  | pending | `tools/web_search.lua`                                          |
| `tools/web_shared.rs`          | 436  | Shared HTTP helpers            | moved-to-capability | P3.a  | pending | → `tui::http` (fetch + redirects, the bulk of the module)                        |
| `tools/write_file.rs`          | 190  | Write file tool                | moved-to-lua | P5.b  | pending | `tools/write_file.lua`                                          |

## `crates/protocol/src/`

| File         | LOC | Purpose                                 | Fate         | Phase | Status  | Notes                                                                                                |
| ------------ | --- | --------------------------------------- | ------------ | ----- | ------- | ---------------------------------------------------------------------------------------------------- |
| `content.rs` | 171 | Content / ContentPart types             | kept         | none  | pending | Wire types                                                                                           |
| `event.rs`   | 414 | EngineEvent + UiCommand                 | restructured | P5.c/P5.e | pending | P5.c: drop `EngineEvent::{AgentMessage, AgentExited, Spawned}`, `UiCommand::AgentMessage`, and `AgentBlockData`. P5.e: rename pass (`SetMode`→`SetAgentMode`, `ExecutePluginTool`→`ToolDispatch`, etc.) |
| `lib.rs`     | 26  | Re-exports                              | kept         | none  | pending |                                                                                                      |
| `message.rs` | 200 | Message / Role / ToolCall / ToolOutcome | restructured | P5.c  | pending | Drop `Role::Agent` — sub-agent replies are `Role::Tool` results                                      |
| `mode.rs`    | 116 | `Mode` enum + ReasoningEffort           | renamed      | P5.e  | pending | `Mode` → `AgentMode` to match diagram                                                                |
| `usage.rs`   | 102 | TokenUsage / TurnMeta / overrides       | kept         | none  | pending | Wire types                                                                                           |

## `runtime/lua/smelt/`

| File                              | LOC | Purpose                               | Fate         | Phase | Status  | Notes                                                              |
| --------------------------------- | --- | ------------------------------------- | ------------ | ----- | ------- | ------------------------------------------------------------------ |
| `_bootstrap.lua`                  | 71  | Yield primitives, fuzzy ranking       | kept         | none  | pending | Autoload                                                           |
| `cmd.lua`                         | 77  | Command framework wrappers            | kept         | P4.e  | pending | Possibly merges into `commands.lua`; decide while coding           |
| `confirm.lua`                     | 136 | Confirm dialog renderer               | moved        | P4.d  | pending | → `dialogs/confirm.lua`. C.9b: reads reason via panel handle's `:text()`; tracks `selected_idx` via `selection_changed`; tracks `typed_reason` via `text_changed`. |
| `dialog.lua`                      | 269 | Dialog handle factory + framework     | kept         | P4.a  | pending | Primary dialog abstraction. C.9b: panel handles capture leaf WinId from `named_inputs`; `kind = "input"` panels expose `:text()`; `:focus()` routes to `smelt.win.set_focus` for input panels. |
| `picker.lua`                      | 68  | Generic picker helper                 | moved        | P4.a  | pending | → `widgets/picker.lua`                                             |
| `prompt_picker.lua`               | 164 | Prompt-docked picker                  | moved        | P4.a  | pending | → `widgets/prompt_picker.lua` (or merge into `widgets/picker.lua`) |
| `plugins/agents.lua`              | 248 | `/agents` dialog                      | moved        | P4.d  | pending | → `dialogs/agents.lua`                                             |
| `plugins/ask_user_question.lua`   | 119 | ask_user_question tool                | moved        | P5.b  | pending | → `tools/ask_user_question.lua`                                    |
| `plugins/background_commands.lua` | 224 | Bash run-in-background tools + `/ps`  | restructured | P5.b  | pending | Tool registrations → `tools/`; `/ps` → `commands.lua` or stays     |
| `plugins/btw.lua`                 | 58  | `/btw` side-question                  | moved        | P4.e  | pending | → `commands.lua` (or stays as plugin)                              |
| `plugins/color.lua`               | 30  | `/color` slug accent                  | moved        | P4.e  | pending | → `commands.lua`                                                   |
| `plugins/export.lua`              | 159 | `/export` markdown                    | moved        | P4.e  | pending | → `commands.lua`                                                   |
| `plugins/help.lua`                | 47  | `/help` keybinding viewer             | moved        | P4.e  | pending | → `commands.lua`                                                   |
| `plugins/history_search.lua`      | 50  | Ctrl+R history search                 | moved        | P4.e  | pending | → `commands.lua`                                                   |
| `plugins/model.lua`               | 37  | `/model` picker                       | moved        | P4.e  | pending | → `commands.lua`                                                   |
| `plugins/permissions.lua`         | 97  | `/permissions` dialog                 | moved        | P4.d  | pending | → `dialogs/permissions.lua`                                        |
| `plugins/plan_mode.lua`           | 181 | Plan mode hooks + exit_plan_mode tool | restructured | P5.b  | pending | Tool → `tools/`; mode hook stays in `modes.lua` or `plugins/`      |
| `plugins/predict.lua`             | 71  | Input prediction                      | kept         | P4    | pending | Stays as plugin (Lua-only feature)                                 |
| `plugins/resume.lua`              | 172 | `/resume` session picker              | moved        | P4.d  | pending | → `dialogs/resume.lua`                                             |
| `plugins/rewind.lua`              | 57  | `/rewind` turn picker                 | moved        | P4.d  | pending | → `dialogs/rewind.lua`                                             |
| `plugins/settings.lua`            | 38  | `/settings` toggles                   | moved        | P4.e  | pending | → `commands.lua`                                                   |
| `plugins/theme.lua`               | 32  | `/theme` accent picker                | moved        | P4    | pending | → `colorschemes/<n>.lua` + `commands.lua` thin wrapper             |
| `plugins/toggles.lua`             | 13  | `/vim`, `/thinking` toggles           | moved        | P4.e  | pending | → `commands.lua`                                                   |
| `plugins/yank_block.lua`          | 12  | Optional `/yank-block`                | kept         | P4    | pending | Stays as opt-in plugin                                             |

**To be created (P4.a):**

- `widgets/{input,options,list,picker,cmdline,statusline,notification}.lua`
- `dialogs/{confirm,permissions,agents,rewind,resume}.lua`
- `colorschemes/<default>.lua`
- `transcript.lua`, `diff.lua`, `status.lua`, `modes.lua`, `commands.lua`

**To be created (P5.b) under `tools/`:**

- `bash.lua`, `read_file.lua`, `write_file.lua`, `edit_file.lua`, `glob.lua`,
  `grep.lua`, `web_fetch.lua`, `web_search.lua`, `notebook_edit.lua`,
  `spawn_agent.lua`, `stop_agent.lua`, `message_agent.lua`, `peek_agent.lua`,
  `list_agents.lua`, `load_skill.lua`, `ask_user_question.lua` (moved from
  plugins), `exit_plan_mode.lua` (extracted from `plan_mode.lua`)

## `src/`

| File         | LOC | Purpose                               | Fate | Phase | Status  | Notes                                 |
| ------------ | --- | ------------------------------------- | ---- | ----- | ------- | ------------------------------------- |
| `main.rs`    | 690 | CLI entry, args, dispatch             | kept | P2    | pending | Minor edits for App carve-up          |
| `setup.rs`   | 277 | Interactive setup flows               | kept | none  | pending | dialoguer-based first-run wizard      |
| `startup.rs` | 403 | Resolve config/keys/models pre-engine | kept | P2    | pending | Produces ResolvedStartup for App init |

## To be created (Rust capability modules, P3.a)

Live as `crates/tui/src/<name>.rs` (or small folder if needed):

| Module          | Purpose                           | Composes from today's                                                   |
| --------------- | --------------------------------- | ----------------------------------------------------------------------- |
| `tui::parse`    | markdown / diff / syntax          | `content/highlight/{inline,diff,syntax,util}` + `content/stream_parser` |
| `tui::process`  | short-lived spawn / streaming / kill | `engine/tools/background` + per-tool subprocess code (foreground)   |
| `tui::subprocess` | long-lived child: spawn · send · on_event · wait · kill | `engine/socket.rs` + `engine/registry.rs` + the multi-agent IPC pieces of `engine/agent.rs` |
| `tui::fs`       | read / write / edit / glob / lock | `engine/tools/{read_file,write_file,edit_file,glob,file_state}`         |
| `tui::http`     | fetch / cache / redirects         | `engine/tools/{web_cache,web_shared}`                                   |
| `tui::html`     | html → markdown                   | extracted from `engine/tools/web_fetch`                                 |
| `tui::notebook` | Jupyter JSON ops                  | extracted from `engine/tools/notebook`                                  |
| `tui::grep`     | ripgrep wrapper                   | `engine/tools/grep`                                                     |
| `tui::path`     | normalize / canonical / relative  | new (small)                                                             |
| `tui::fuzzy`    | fuzzy matching/scoring            | `tui/fuzzy` + `tui/completer/score`                                     |
| `tui::permissions` | all permission policy: bash AST · pattern match · workspace check · runtime approvals · workspace JSON store | folds entire `engine/permissions/{approvals,bash,rules,workspace,tests}.rs` + `tui/workspace_permissions.rs` |

## Unclear — needs explicit decision

| File / question                                                                           | Phase needed by | Decision needed                                                                                                                                           |
| ----------------------------------------------------------------------------------------- | --------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `runtime/lua/smelt/cmd.lua`                                                               | P4.e            | Merge into `commands.lua`, or keep as the framework helper that `commands.lua` calls?                                                                     |
| `runtime/lua/smelt/prompt_picker.lua`                                                     | P4.a            | Merge into `widgets/picker.lua`, or stay separate as `widgets/prompt_picker.lua`? Decide while coding P4.a.                                              |
| `plugins/{btw, color, export, help, history_search, model, settings, theme, toggles}.lua` | P4.e            | All move to `commands.lua` (one master file), or stay as one-file-per-command in `plugins/` with `commands.lua` as registration index?                    |
| `plugins/predict.lua`                                                                     | P4              | Stay in `plugins/` (it's a hook, not a command/dialog/tool), or move under a new `hooks/` dir?                                                            |
| `plugins/plan_mode.lua`                                                                   | P4/P5           | Split: tool → `tools/exit_plan_mode.lua`, hook → `modes.lua` or `plugins/`. Confirm split shape.                                                          |
| `lua/render_ops.rs` (smelt.diff/syntax/bash/notebook.render)                              | P3.b            | Live under one binding file each (`parse.rs`?), or per-language as-is (`diff.rs`/`syntax.rs`)?                                                            |

