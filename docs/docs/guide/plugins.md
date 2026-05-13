# Plugin Authoring

smelt's runtime exposes a Lua API on the global `smelt` table — register
commands, react to window events, paint extmarks, read or mutate the prompt
buffer, talk to the engine, and so on. Plugins are plain Lua files dropped into
`~/.config/smelt/init.lua` (or required from there).

The full surface lives in the [Lua API reference](../reference/api/index.md);
this page covers the workflow for writing plugins against it.

## IDE completion

Every public function, opts record, and string-literal alias is generated from
the Rust source by `cargo run -p tui --bin gen-lua-docs`. The output lands in
two trees:

- `runtime/lua/smelt/_meta/<namespace>.lua` — `---@meta` stubs consumed by
  [lua-language-server](https://github.com/LuaLS/lua-language-server) for
  completion, hover docs, and type checks.
- `runtime/lua/smelt/_meta/_types.lua` — shared `---@class` records and
  `---@alias` string-literal unions referenced from the per-namespace stubs.

Point lua-language-server at the `_meta` directory in your editor. The shipped
[`runtime/.luarc.json`](https://github.com/leonardcser/smelt/blob/main/runtime/.luarc.json)
is a working config — copy it next to your `init.lua` (or set
`workspace.library` to the smelt checkout's `runtime/lua/smelt/_meta` path).
With that in place, typing `smelt.win.on_event(` brings up the
`smelt.win.Event` string-literal union, and `smelt.buf.set_extmark` autocompletes
each field of `smelt.buf.ExtmarkOpts`.

## Host vs UiHost

Bindings are tagged with one of two tiers. The
[Lua API index](../reference/api/index.md) groups namespaces by tier and each
per-namespace page calls it out in the header.

- **Host** — works everywhere, including headless mode (`smelt --headless`).
  Examples: `smelt.fs`, `smelt.http`, `smelt.process`, `smelt.cell`,
  `smelt.au`.
- **UiHost** — requires a live terminal UI. Calling a UiHost function from
  headless mode raises. Examples: `smelt.win`, `smelt.buf`, `smelt.theme`,
  `smelt.confirm`, `smelt.statusline`.

Plugins that should keep working in headless contexts must avoid UiHost
namespaces or guard them behind a tier check.

## A small example

```lua
-- ~/.config/smelt/init.lua
local prompt = smelt.prompt.win_id()

local ns = smelt.buf.create_namespace("hello")

-- Highlight the prompt buffer's first row whenever its text changes.
smelt.win.on_event(prompt, "text_changed", function()
  local buf = smelt.win.buf(prompt)
  if not buf then return end
  smelt.buf.clear_namespace(buf, ns)
  smelt.buf.set_extmark(buf, ns, 1, 0, {
    end_col = 999,
    hl_group = "DiagnosticHint",
    priority = 200,
  })
end)
```

Both the `"text_changed"` literal and the `set_extmark` opts table get
type-checked: an unknown event name or a typo'd field surfaces as a diagnostic
in your editor before the plugin ever runs.

## String-literal aliases

String parameters typed as `smelt.<namespace>.<Name>` accept a closed set of
labels — the IDE shows them in autocomplete and rejects typos. Closed aliases
require canonical names only: `smelt.vim.set_mode("normal")` works,
`smelt.vim.set_mode("n")` no longer does (short forms `"n"`, `"i"`, `"v"`,
`"V"`, and PascalCase variants like `"Insert"` are not accepted as of the
typed-alias migration). Open aliases (e.g. [`smelt.cell.Name`](../reference/api/types.md#smeltcellname))
keep accepting any string and just expose well-known names as completion
hints.

## Regenerating docs and stubs

```bash
cargo run -p tui --bin gen-lua-docs
```

Run this after editing any `register_fn`/`register_ui_fn` site or an opts
struct. It rewrites `runtime/lua/smelt/_meta/`, the
`docs/docs/reference/api/` Markdown pages, and the navigation block in
`docs/zensical.toml`.
