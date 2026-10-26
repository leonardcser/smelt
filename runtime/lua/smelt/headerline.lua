-- Headerline window - Lua-allocated, Lua-rendered top chrome.
--
-- Features register named sources with `smelt.headerline.add(name, spec)`. Each spec may
-- expose:
--   * visible() -> boolean      whether this source currently reserves a row
--   * render(width) -> row      returns `{ text, highlights }` for one row
--
-- The default layout reserves this window above the transcript when any source
-- is visible. The first visible source owns the row.

local bar = require("smelt._bar")

local M = {}
smelt.headerline = M

local NS = smelt.ns("smelt.headerline")
local sources = {} -- ordered { name, spec } pairs
local renderer_subscriptions = {}

-- Repaint and recompose the layout after plugin-owned state changes that are
-- not represented by a built-in signal. Visibility may change the row count.
-- Adding, replacing, and removing sources invalidates automatically.
---@tier ui_host
---@type fun()
function smelt.headerline.invalidate()
  if M.win then M.win:invalidate_renderer() end
  smelt.ui.layout.invalidate()
end

--- One header row: its text and the highlight ranges to paint over it.
---@class smelt.headerline.Row
---@field text string Row text.
---@field highlights? smelt.headerline.Highlight[] Highlights over `text`.

--- A highlight over a header row, in byte offsets into the row text.
---@class smelt.headerline.Highlight
---@field bytes_start integer Start byte offset (inclusive).
---@field bytes_end integer End byte offset (exclusive).
---@field style smelt.buf.MarkOpts Style to apply.
---@field selectable? boolean Include the range in drag-copy.

--- A headerline source. The first source whose `visible` returns true owns the row.
---@class smelt.headerline.Spec
---@field render fun(width: integer): smelt.headerline.Row Build the row for a given content width.
---@field visible? fun(): boolean Whether this source currently reserves the row. Defaults to always visible.

--- Add a named headerline source; a bare function is shorthand for `{ render = fn }`.
--- The header takes a row above the transcript only while some source is visible.
--- Adding a name that already exists replaces that source in place. Returns a
--- `Reg` that removes this source, unless a later `add` with the same name has
--- replaced it.
---@tier ui_host
---@type fun(name: string, spec: smelt.headerline.Spec|fun(width: integer): smelt.headerline.Row): smelt.Reg
function smelt.headerline.add(name, spec)
  if type(name) ~= "string" or name == "" then
    error("smelt.headerline.add: name must be a non-empty string", 2)
  end
  if type(spec) == "function" then spec = { render = spec } end
  if type(spec) ~= "table" or type(spec.render) ~= "function" then
    error("smelt.headerline.add: spec must be a table with render(width)", 2)
  end
  local entry = { name = name, spec = spec }
  local replaced = false
  for i, src in ipairs(sources) do
    if src.name == name then
      sources[i] = entry
      replaced = true
      break
    end
  end
  if not replaced then sources[#sources + 1] = entry end
  M.invalidate()
  return smelt.reg.new(function()
    for i, src in ipairs(sources) do
      if src == entry then
        table.remove(sources, i)
        M.invalidate()
        return
      end
    end
  end)
end

--- Remove every source named `name`.
---@tier ui_host
---@type fun(name: string)
function smelt.headerline.remove(name)
  for i = #sources, 1, -1 do
    if sources[i].name == name then table.remove(sources, i) end
  end
  M.invalidate()
end

local function is_visible(src)
  if type(src.spec.visible) ~= "function" then return true end
  local ok, result = pcall(src.spec.visible)
  if not ok then
    io.stderr:write("smelt.headerline source `" .. src.name .. "`: " .. tostring(result) .. "\n")
    return false
  end
  return result == true
end

local function visible_source()
  for _, src in ipairs(sources) do
    if is_visible(src) then return src end
  end
  return nil
end

function M.rows()
  return visible_source() and 1 or 0
end

local function render(win)
  local buf = win:buf()
  if not buf then return end
  local src = visible_source()
  if not src then
    bar.write_rows(buf, { { text = "", highlights = {} } }, NS)
    return
  end
  local ok, row = pcall(src.spec.render, win:content_width() or 80)
  if not ok then
    io.stderr:write("smelt.headerline source `" .. src.name .. "`: " .. tostring(row) .. "\n")
    row = { text = "", highlights = {} }
  end
  if type(row) ~= "table" or type(row.text) ~= "string" then
    row = { text = "", highlights = {} }
  end
  bar.write_rows(buf, { row }, NS)
end

M.win = smelt.win.new(smelt.buf.new({ name = "smelt.headerline" }), {
  name = "smelt.headerline",
  scrollbar = false,
  surface = "selectable_text",
  region = "header",
})

if M.win then
  M.win:set_renderer(render)
  if type(smelt.signal.subscribe) == "function" then
    renderer_subscriptions[#renderer_subscriptions + 1] = smelt.signal.subscribe(
      "session_epoch", M.invalidate)
  end
end

return M
