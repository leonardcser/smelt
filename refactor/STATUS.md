# Status

Where we are right now. Updated at session end.

For the entry point and meta-rules, read `README.md` first.

## Where we are

**Phase:** P1.0 theme registry landed end-to-end. P1.a in progress:
extmark + namespace model landed in `ui::Buffer` as the primary
storage; `YankSubst` extmark field + `Buffer::yank_text_for_range`
helper added; soft-wrap cache (`Buffer::wrap_at`) keyed by
`(changedtick, width)` added. Per-line `add_highlight` /
`set_decoration` / `set_virtual_text` / `set_mark` are now thin
wrappers over `set_extmark` in well-known namespaces. Foundation
commit 1 of the `Buffer::attach(spec)` parser-hook migration landed:
`BufferFormatter` → `BufferParser` rename + `on_attach` lifecycle
hook + builder API renamed to `Buffer::attach(parser)` /
`set_parser`. The `edit_buffer.rs` merge is still pending in P1.a.

**Tree:** green. `cargo nextest run --workspace` — 930 passed (914
from prior boundary + 16 new tests covering extmark CRUD, yank
substitution, and wrap caching; renamed-tests preserved). `cargo
nextest run --test scenarios` — 6 baseline scenarios green. `cargo
clippy` clean.

**Last update:** 2026-04-29. P1.0 theme registry landing across 12
commits (`decb0ab`..`e489a79`):

- `decb0ab` — `ui::Theme` registry type (HashMap groups + links).
- `177ac4c` — plumbed through `DrawContext`; `Ui` owns it.
- `bb9cc63` — `populate_ui_theme()` mirrors host constants.
- `9bf1912` — first call site batch: render_loop, events,
  status_bar, ui_ops.
- `31cfb56` — renamed `crate::theme::Theme` snapshot → `Snapshot`.
- `d92a715` — status separator color + notification error_label.
- `da75d3a` — re-populate registry each frame so Lua-driven mutations
  propagate without a separate notification path.
- `387f4d2` — replaced `crate::theme::Snapshot` with `&ui::Theme` in
  the entire render pipeline; deleted the snapshot type.
- `1786716` — added `ColorRole::Agent / Success / ErrorMsg`; migrated
  `transcript_present/*` renderers off the const colors.
- `16beb71` — threaded `&ui::Theme` through `compute_prompt` /
  `reasoning_color` / `WindowView::draw_scrollbar`; migrated
  `confirm_preview` notebook title; added `ColorRole::Apply / Plan /
  Exec / Heading / ReasonLow / ReasonMed / ReasonHigh / ReasonMax`.
- `7aadcd2` — threaded `&ui::Theme` through
  `WindowView::set_soft_cursor` (last renderer-side `theme::is_light`
  caller).
- `e489a79` — collapsed atomics into `ui::Theme`. `accent`, `slug`,
  `is_light` live as fields with proper accessors; the per-frame
  `populate_ui_theme()` reads them and rewrites the `Smelt*` groups.
  Lua API closures access theme via `with_app(|app| app.ui.theme())`
  — no global state on either side. Inline ANSI helpers in
  `headless.rs` use literal `Color::Red` / `Color::AnsiValue(77)`
  rather than const aliases. Syntect's light/dark hint moved into
  `content/highlight/mod.rs` as a self-contained mirror updated by
  `populate_ui_theme()` (avoids threading `&Theme` through 14 syntax
  call sites for one branch).
- `50b8923` — **P1.a kickoff**: extmark + namespace model in
  `ui::Buffer`. New API: `create_namespace`, `set_extmark`,
  `del_extmark`, `clear_namespace`, `extmarks(ns)`. Single
  `BTreeMap<ExtmarkId, Extmark>` per namespace. The convenience
  methods `add_highlight` / `set_decoration` / `set_virtual_text` /
  `set_mark` create extmarks in the well-known namespaces
  (`NS_HIGHLIGHTS`, `NS_DECORATIONS`, `NS_VIRT_TEXT`, `NS_MARKS`).
  Per-line getters (`highlights_at`, `decoration_at`,
  `virtual_text_at`, `get_mark`) read directly from the extmark
  store. `highlights_arc` / `decorations_arc` materialize on demand
  with a `(changedtick + marks_tick)` cache so `BufferView::sync_from_buffer`
  still gets `Arc::clone` semantics. `sync_from_buffer` and
  `build_panels` switched to `&mut Buffer` / `&mut dyn BufferResolver`
  to thread the lazy materialization.
- `67603de` — `YankSubst` extmark field +
  `Buffer::yank_text_for_range(start_row, start_col, end_row, end_col)`.
  An extmark with `yank: Some(YankSubst::Empty)` elides covered
  bytes on yank; `Some(YankSubst::Static(s))` substitutes them.
  Walks every namespace, sorts substitutions in source order,
  emits literal text for uncovered bytes. No callers yet — building
  block for hidden-thinking elision and prompt attachment expansion.
- `26701d3` — `Buffer::wrap_at(width)` soft-wrap cache keyed by
  `(changedtick, width)`. Reuses the result across repeated calls
  and (eventually) across multiple Windows on the same Buffer.
  Cache invalidates on any line mutation. No callers yet — wrap
  state today still lives in WindowView; migration is downstream
  P1.a work.
- `385e9d0` — **`Buffer::attach(spec)` foundation commit 1**:
  `BufferFormatter` trait → `BufferParser`; `render` method →
  `parse`; `with_formatter` builder → `attach`; `set_formatter` →
  `set_parser`. New `BufferParser::on_attach(&mut Buffer)`
  lifecycle hook (default no-op) fires once when the parser is
  installed — entry point for parsers to register custom
  namespaces and seed initial state. Tests + 4 call sites updated
  (`format.rs` ModeFormatter → ModeParser, `lua/api/widgets.rs`,
  `lua/ui_ops.rs`). Pure rename + one new hook; no behavior
  change. Sets up the API shape for the deeper parser-hook surgery
  (incremental `on_change` / `on_render` hooks) without committing
  to the full restructure.

`crate::theme::*` is now narrow:
- `populate_ui_theme(&mut Theme)` — initializes `Smelt*` highlight
  groups from `theme.is_light()` + `theme.accent()` + `theme.slug()`.
- `detect_background(&mut Theme)` — OS-level OSC 11 / `$COLORFGBG`
  probe; sets `theme.set_light(...)` if successful.
- `PRESETS` — preset accent picker list (12 colors).
- `preset_by_name(&str) -> Option<u8>` — Lua API helper.
- `DEFAULT_ACCENT` re-export from `ui::theme::DEFAULT_ACCENT`.

P0 closed: 4 of 9 deletions shipped (orthogonal); 5 structural
deletions still deferred to later P1 sub-phases (paired with
replacements):
- `BufferView` deletion paired with `Window::render(buf, grid)` —
  P1.d.
- `PanelWidget` trait + dialog.rs panel multiplexing — P1.c.
- `Component` trait + remaining `WidgetEvent` — P1.d.
- `Placement` enum + `add_layer`/`register_split` plumbing — P1.b +
  P1.c.

**Note for next session:** puml + SVG are in sync. If the puml is
edited, regenerate via `plantuml -tsvg
refactor/tui-ui-architecture-target.puml`. Run `refactor/check.sh`
before declaring anything done.

## What's next

`Buffer::attach(spec)` parser-hook migration is in progress —
commit 1 (rename + `on_attach`) landed at `385e9d0`. The remaining
commits in this series are each a meaningful slice on their own
and can be tackled independently:

1. **Commit 2 — `on_change(buf, line_range)` hook.** Add the
   incremental-edit hook to `BufferParser`; default implementation
   delegates to a fresh `parse(&source, width)`. No consumer needs
   to change immediately, but the hook unblocks streaming-parse
   parsers down the line. ~1 hour.
2. **Commit 3 — `on_render(buf, width)` hook.** Move the
   width-dependent half of `parse` into a separate hook so parsers
   can short-circuit when only the width changed. Lets the
   markdown / syntect parsers cache the width-independent IR
   (replaces what `transcript_cache.rs` does today). ~Half-day; the
   IR cache split lives here.
3. **Commit 4 — Delete `transcript_cache.rs`.** Once `on_render`
   exists, the IR cache becomes per-parser state inside the
   markdown / inline-diff parsers. The persisted layout cache stays
   (separate concern). Cascades through the engine bridge and
   resume path. ~1 day.

Two other P1.a items, independent of the parser-hook series:

- **Edit history merge**. Roll `edit_buffer.rs` (`EditBuffer` +
  per-buffer history + word/line range helpers) into `Buffer`.
  Cascades through `PromptState`, `Window`, every `input/buffer.rs`
  site that reaches `self.win.edit_buf.buf`. ~250 references; ~2
  days.
- **Drop `BufferView` `Arc::clone` of materialized vecs**. Blocked
  on `Component::draw` not having `&Buffer` access — defer to P1.d
  when `BufferView` is deleted outright.

Migrate-on-demand consumers (each surfaces hidden coupling, see
P1.a notes):
- Hidden-thinking blocks → `YankSubst::Empty` requires re-rooting
  `TranscriptSnapshot` in a Buffer; not 1 file.
- Prompt attachment sigils → `YankSubst::Static` is a wrong fit:
  attachments substitute on submit, not on copy.
- WindowView wrap → `Buffer::wrap_at` is a wrong fit: rendering
  pre-wraps via `DisplayLine`s before the buffer; `wrap_at`
  operates after the fact.

Subsequent P1 sub-phases (after P1.a closes):
- **P1.b — `LayoutTree`** (`Vbox`/`Hbox`/`Leaf(WinId)` + constraints).
- **P1.c — `Overlay` replacing `Float`** (deletes `PanelWidget` +
  `dialog.rs` multiplexing).
- **P1.d — `Window` as only interactive unit** (deletes `Component`
  trait + `BufferView`; vim/completer state machines decompose).

Subsequent P1 sub-phases (after P1.a closes):
- **P1.b — `LayoutTree`** (`Vbox`/`Hbox`/`Leaf(WinId)` + constraints).
- **P1.c — `Overlay` replacing `Float`** (deletes `PanelWidget` +
  `dialog.rs` multiplexing).
- **P1.d — `Window` as only interactive unit** (deletes `Component`
  trait + `BufferView`; vim/completer state machines decompose).

Recently shipped: theme registry + plumbing + atomic-on-Theme
collapse + 42 call site migrations + Snapshot elimination + ColorRole
expansion + Buffer extmark + namespace model.
P0 orthogonal deletions
(`selection_style`/`set_selection_bg` shim, `handle_mouse_with_lua` +
`classify_widget_action`, `MouseAction::Yank`/`WidgetEvent::Yank`,
`BufferList`). `TESTING.md` (three-layer testing strategy). Test
harness + 5 baseline scenarios (`plain_turn`, `thinking_then_text`,
`incomplete_stream`, `provider_auth_error`, `streaming_concat_across_deltas`).
`TRACE.md` vertical-slice walk-through.

## Open questions / blocked

(Rows from `INVENTORY.md` that need a decision before their phase
begins. Move resolved items down to a "decisions" log here or in the
relevant `P<n>.md`.)

- **`commands.lua` shape** — P4.e. One master file vs one-per-command in
  `plugins/`. Decide while coding P4.e.
- **`render_ops.rs` per-language split** — P3.b. One `parse.rs` binding,
  or per-language (`diff.rs`/`syntax.rs`)? Decide while coding P3.b.
- **`prompt_picker.lua`** — P4.a. Merge into `widgets/picker.lua` or stay
  separate as `widgets/prompt_picker.lua`? Decide while coding P4.a.
- **`predict.lua` location** — P4. Stay in `plugins/` (it's a hook), or
  move under a new `hooks/` dir? Decide while coding P4.
- **`plan_mode.lua` split** — P4/P5. Tool half → `tools/exit_plan_mode.lua`,
  hook half → `modes.lua` or `plugins/`. Confirm split shape when P4/P5
  begin.

None of these block the start of P0 or P1.

## Recently landed

(Phases as they complete. Newest first. One line each, link to the
P-log.)

- **P0 — Clear the deck (orthogonal half).** 4 of 9 deletions
  shipped; 5 structural deletions deferred to P1.0. Tree green; 901
  workspace tests + 6 baseline scenarios passing. See `P0.md`.

## How to keep this file current

- At session end: update "Where we are", "What's next", "Open
  questions" — even one line is enough.
- When a phase boundary lands: rotate the line into "Recently landed",
  reset "What's next."
- When a decision is made: log it in the active `P<n>.md`; if it
  changes scope, also update `REFACTOR.md` in the same commit.

(For meta-rules and the cold-start checklist, see `README.md`.)
