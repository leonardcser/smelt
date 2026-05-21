-- Statusline window. Replaces the old `status.lua` + Rust-side
-- `smelt.statusline.register/unregister/snapshot` plumbing.
--
-- This module owns:
--   * the statusline window (`M.win`) — Lua-allocated
--   * a registry of named source callbacks (`M.add` / `M.remove`)
--   * a per-frame renderer that calls each source, flattens the
--     returned segments, composes via `_bar.compose_status`, and
--     writes to the window's buffer.
--
-- The built-in `core` source reads engine state directly from cells
-- (`vim_mode`, `agent_mode`, `tps`, `task_label`, `running_procs`,
-- `permission_pending`, `cursor_pos`) and `smelt.session.tokens()` for
-- cache info. Plugins extend the line by registering additional
-- sources via `M.add(name, fn)`.

local bar = require("smelt._bar")

local M = {}

local NS = smelt.ns("smelt.statusline")
local sources = {} -- ordered { name, handler } pairs

-- ── source registry ─────────────────────────────────────────────────

function M.add(name, handler)
  for _, src in ipairs(sources) do
    if src.name == name then
      src.handler = handler
      return
    end
  end
  sources[#sources + 1] = { name = name, handler = handler }
end

M.register = M.add

function M.remove(name)
  for i = #sources, 1, -1 do
    if sources[i].name == name then table.remove(sources, i) end
  end
end

-- ── default `core` source ───────────────────────────────────────────

local function vim_group(label)
  if label == "INSERT" then return "SmeltVimInsert"
  elseif label == "VISUAL" or label == "VISUAL LINE" then return "SmeltVimVisual"
  else return "SmeltVimNormal"
  end
end

local function agent_mode_group(name)
  if name == "plan" then return "SmeltModePlan"
  elseif name == "apply" then return "SmeltModeApply"
  elseif name == "yolo" then return "SmeltModeYolo"
  elseif name == "exec" then return "SmeltModeExec"
  else return "SmeltModeDefault"
  end
end

local function cell(name) return smelt.cell(name):get() end

local function core_compose()
  local items = {}

  -- Slug pill.
  local task_label = cell("task_label")
  if smelt.settings.show_slug and task_label and task_label ~= "" then
    items[#items + 1] = {
      text = " " .. task_label .. " ",
      style = { hl_group = "SmeltSlug" },
      priority = 5,
      truncatable = true,
    }
  end

  -- Vim mode pill.
  local vim_label = cell("vim_mode")
  if vim_label and vim_label ~= "" then
    items[#items + 1] = {
      text = " " .. vim_label .. " ",
      style = { hl_group = vim_group(vim_label) },
      priority = 3,
    }
  end

  -- Agent mode pill.
  local mode_name = cell("agent_mode")
  if mode_name and mode_name ~= "" then
    local icon = smelt.mode.icon and smelt.mode.icon(mode_name) or ""
    items[#items + 1] = {
      text = " " .. icon .. mode_name .. " ",
      style = { hl_group = agent_mode_group(mode_name) },
      priority = 1,
    }
  end

  -- tok/s.
  local tps = cell("tps") or 0
  if smelt.settings.show_tps and tps > 0 then
    items[#items + 1] = {
      text = string.format(" %.1f tok/s", tps),
      style = { fg = "Comment" },
      priority = 4,
    }
  end

  -- Cache hit ratio (grouped with token indicators; hides when show_tokens is off).
  if smelt.settings.show_tokens then
    local tokens = smelt.session.tokens()
    if tokens and tokens.cache_hit_ratio then
      local pct = math.floor(tokens.cache_hit_ratio * 100 + 0.5)
      items[#items + 1] = {
        text = "cache " .. pct .. "%",
        style = { fg = "Comment" },
        priority = 4,
        separated = true,
      }
    end
  end

  -- Right-strip indicators.
  if cell("permission_pending") then
    items[#items + 1] = {
      text = "permission pending",
      style = { fg = "SmeltAccent", bold = true },
      priority = 2,
      separated = true,
    }
  end

  local procs = cell("running_procs") or 0
  if procs > 0 then
    items[#items + 1] = {
      text = procs == 1 and "1 proc" or (procs .. " procs"),
      style = { fg = "SmeltAccent" },
      priority = 2,
      separated = true,
    }
  end

  local pos = cell("cursor_pos")
  if pos and pos.line and pos.line > 0 then
    items[#items + 1] = {
      text = string.format("%d:%d %d%%", pos.line, pos.col or 1, pos.scroll_pct or 0),
      style = { fg = "Comment" },
      priority = 3,
      align_right = true,
    }
  end

  return items
end

M.add("core", core_compose)

-- ── renderer ────────────────────────────────────────────────────────

local function flatten()
  local items = {}
  for _, src in ipairs(sources) do
    local ok, result = pcall(src.handler)
    if ok and type(result) == "table" then
      for _, it in ipairs(result) do
        items[#items + 1] = it
      end
    elseif not ok then
      io.stderr:write("smelt.statusline source `" .. src.name .. "`: " .. tostring(result) .. "\n")
    end
  end
  return items
end

local function render(win)
  local buf = win:buf()
  if not buf then return end
  local width = win:content_width() or 80
  local items = flatten()
  local row = bar.compose_status(items, {
    width = width,
    bg_group = "SmeltStatusBg",
    sep_group = "Comment",
  })
  bar.write_rows(buf, { row }, NS)
end

-- ── window allocation ───────────────────────────────────────────────

M.win = smelt.win.new(smelt.buf.new({ name = "smelt.statusline" }), {
  name = "smelt.statusline",
  scrollbar = false,
  focusable = false,
  region = "status",
})

if M.win then M.win:set_renderer(render) end

-- Row count this composer paints into. Layout / overlay code that needs
-- to reserve space above the statusline reads this instead of touching
-- the window rect directly (the rect is only valid after the first
-- render, so an overlay opening on cold start would see nil). A plugin
-- replacing this module is free to override `M.rows` if it produces a
-- multi-row statusline.
M.rows = 1

return M
