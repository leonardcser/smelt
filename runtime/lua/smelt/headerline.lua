-- Headerline window - Lua-allocated, Lua-rendered top chrome.
--
-- A strip bar like the statusline, with the same item sources: features add
-- named sources with `smelt.headerline.add(name, handler)`, and each returns
-- `smelt.bar.Item`s. The default layout reserves rows above the transcript
-- only while some source returns a visible item.

local bar = require("smelt._bar")

local M = {}
smelt.headerline = M

local NS = smelt.ns("smelt.headerline")
local renderer_subscriptions = {}

local line = bar.new({
  name = "smelt.headerline",
  layout = "strip",
  min_rows = 0,
  -- A header is content (a goal, a title), so drag-copy takes the whole row.
  fill_selectable = true,
})

--- Repaint after plugin-owned state changes that are not represented by a
--- built-in signal; the layout follows when the row count changes. Adding,
--- replacing, and removing sources invalidates automatically.
---@tier ui_host
---@type fun()
function smelt.headerline.invalidate()
  bar.invalidate(line)
end

--- Repaint after `ms` milliseconds. An animated source calls it on every paint
--- to ask for its next frame; the earliest pending request wins.
---@tier ui_host
---@type fun(ms: integer)
function smelt.headerline.invalidate_after(ms)
  bar.invalidate_after(line, ms)
end

--- Add a named source of header items. `handler` runs on every repaint and
--- returns a list of items (see `smelt.bar.Item`), or nothing to take no room.
--- The header is as tall as its lowest visible item row and disappears when
--- no source shows anything. Adding a name that already exists replaces that
--- source in place. Returns a `Reg` that removes this source, unless a later
--- `add` with the same name has replaced it.
---@tier ui_host
---@type fun(name: string, handler: fun(): smelt.bar.Item[]?): smelt.Reg
function smelt.headerline.add(name, handler)
  return bar.add(line, name, handler)
end

--- Remove every source named `name`.
---@tier ui_host
---@type fun(name: string)
function smelt.headerline.remove(name)
  bar.remove(line, name)
end

--- Rows the header takes: its lowest visible item row, 0 when empty, or the
--- rows passed to `set_view`.
---@tier ui_host
---@type fun(): integer
function smelt.headerline.rows()
  return bar.rows(line)
end

--- Replace the header with any layout node, such as a paint leaf, taking
--- `rows` rows (default 1). Pass `nil` to restore the built-in header.
---@tier ui_host
---@type fun(node: smelt.ui.layout?, rows: integer?)
function smelt.headerline.set_view(node, rows)
  bar.set_view(line, node, rows)
end

--- The header's layout node and row count, for layout composers.
---@tier ui_host
---@type fun(): smelt.ui.layout, integer
function smelt.headerline.node()
  return bar.node(line)
end

local function render(win)
  local buf = win:buf()
  if not buf then return end
  bar.write_rows(buf, bar.rows_for(line, win:content_width() or 80, math.max(bar.rows(line), 1)), NS)
end

M.win = smelt.win.new(smelt.buf.new({ name = "smelt.headerline" }), {
  name = "smelt.headerline",
  scrollbar = false,
  surface = "selectable_text",
  region = "header",
})

line.win = M.win
if M.win then
  M.win:set_renderer(render)
  if type(smelt.signal.subscribe) == "function" then
    renderer_subscriptions[#renderer_subscriptions + 1] = smelt.signal.subscribe(
      "session_epoch", M.invalidate)
  end
end

return M
