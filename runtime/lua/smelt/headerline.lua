-- Headerline window - Lua-allocated, Lua-rendered top chrome.
--
-- Features register named sources with `M.add(name, spec)`. Each spec may
-- expose:
--   * visible() -> boolean      whether this source currently reserves a row
--   * render(width) -> row      returns `{ text, highlights }` for one row
--
-- The default layout reserves this window above the transcript when any source
-- is visible. The first visible source owns the row.

local bar = require("smelt._bar")

local M = {}

local NS = smelt.ns("smelt.headerline")
local sources = {} -- ordered { name, spec } pairs
local renderer_subscriptions = {}

-- Repaint and recompose the layout after plugin-owned state changes that are
-- not represented by a built-in signal. Visibility may change the row count.
-- Adding, replacing, and removing sources invalidates automatically.
function M.invalidate()
  if M.win then M.win:invalidate_renderer() end
  smelt.ui.layout.invalidate()
end

function M.add(name, spec)
  if type(spec) == "function" then spec = { render = spec } end
  if type(spec) ~= "table" or type(spec.render) ~= "function" then
    error("smelt.headerline.add: spec must be a table with render(width)", 2)
  end
  for _, src in ipairs(sources) do
    if src.name == name then
      src.spec = spec
      M.invalidate()
      return
    end
  end
  sources[#sources + 1] = { name = name, spec = spec }
  M.invalidate()
end

function M.remove(name)
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
