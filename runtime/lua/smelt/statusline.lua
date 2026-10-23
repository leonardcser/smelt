-- Statusline window - Lua-allocated, Lua-rendered.
--
-- This module owns:
--   * the statusline window (`M.win`)
--   * a registry of named source callbacks (`smelt.statusline.add` / `remove`)
--   * a retained renderer that calls each source after semantic invalidation,
--     flattens the returned segments, composes via `_bar.compose_status`, and
--     writes to the window's buffer.
--
-- The built-in `core` source reads engine state from signals plus
-- `smelt.session.status()` for values that carry pending/stale markers
-- (`vim_mode`, `agent_mode`, `tps`, `task_label`, `running_procs`,
-- `permission_pending`, `keymap_pending`, `vim_pending_input`, `cursor_pos`, `viewport_pos`). Plugins extend the line by
-- registering additional sources via `smelt.statusline.add(name, fn)`.

local bar = require("smelt._bar")

local M = {}
smelt.statusline = M

local NS = smelt.ns("smelt.statusline")
local sources = {} -- ordered { name, handler } pairs
local renderer_subscriptions = {}

--- One piece of statusline content. Items are laid out left to right, with
--- `align_right` items packed against the right edge. When the line is too
--- narrow, whole items drop from the highest `priority` down, and truncatable
--- items of the highest remaining priority shrink first.
---@class smelt.statusline.Item
---@field text string Text to show.
---@field style? smelt.buf.MarkOpts Highlight applied to the text (`fg`, `bg`, `hl_group`, `bold`, `dim`, `italic`).
---@field priority? integer Drop order when space runs out; higher values drop first. Defaults to 0.
---@field separated? boolean Draw a separator between this item and the previous separated item on the same side.
---@field truncatable? boolean Allow shrinking this item before dropping it.
---@field truncate? "end"|"middle" Where to cut a truncated item. Defaults to `"end"`.
---@field align_right? boolean Pack against the right edge instead of the left.
---@field selectable? boolean Include the text in drag-copy. Defaults to true.

-- Repaint after plugin-owned state changes that are not represented by a
-- built-in signal. Adding, replacing, and removing sources invalidates
-- automatically.
---@tier ui_host
---@type fun()
function smelt.statusline.invalidate()
  if M.win then M.win:invalidate_renderer() end
end

-- ── source registry ─────────────────────────────────────────────────

--- Add a named source of statusline items. `handler` runs on every repaint and
--- returns a list of items; errors are reported on stderr and skip the source.
--- Adding a name that already exists replaces that source in place, keeping its
--- position. Returns a `Reg` that removes this source, unless a later `add` with
--- the same name has replaced it.
---@tier ui_host
---@type fun(name: string, handler: fun(): smelt.statusline.Item[]?): smelt.Reg
function smelt.statusline.add(name, handler)
  if type(name) ~= "string" or name == "" then
    error("smelt.statusline.add: name must be a non-empty string", 2)
  end
  if type(handler) ~= "function" then
    error("smelt.statusline.add: handler must be a function", 2)
  end
  local entry = { name = name, handler = handler }
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
function smelt.statusline.remove(name)
  for i = #sources, 1, -1 do
    if sources[i].name == name then table.remove(sources, i) end
  end
  M.invalidate()
end

-- ── default `core` source ───────────────────────────────────────────

local function vim_group(label)
  if label == "INSERT" then return "SmeltVimInsert"
  elseif label == "VISUAL" or label == "V-LINE" then return "SmeltVimVisual"
  else return "SmeltVimNormal"
  end
end

local function signal(name) return smelt.signal.get(name) end

local function core_compose()
  local items = {}
  local status = smelt.session.status and smelt.session.status() or {}

  -- Slug pill. SmeltSlug carries fg only; fall back to SmeltAccent's
  -- fg as the pill bg when no explicit bg has been set (default), so
  -- `/color` (which writes SmeltSlug.bg) and theme swaps both
  -- propagate naturally.
  local task_label = signal("task_label")
  if smelt.settings.show_slug and task_label and task_label ~= "" then
    local slug = smelt.theme.get("SmeltSlug") or {}
    local style = { hl_group = "SmeltSlug" }
    if not slug.bg then
      local accent = smelt.theme.get("SmeltAccent")
      if accent and accent.fg then
        style.bg = accent.fg
      end
    end
    items[#items + 1] = {
      text = " " .. task_label .. " ",
      style = style,
      priority = 5,
      truncatable = true,
    }
  end

  -- Vim mode pill.
  local vim_label = signal("vim_mode")
  if vim_label and vim_label ~= "" then
    items[#items + 1] = {
      text = " " .. vim_label .. " ",
      style = { hl_group = vim_group(vim_label) },
      priority = 3,
    }
  end

  -- Agent mode pill.
  local mode = status.mode or {}
  local mode_name = mode.name or signal("agent_mode")
  if mode_name and mode_name ~= "" then
    local spec = smelt.mode.get(mode_name) or {}
    items[#items + 1] = {
      text = " " .. (spec.icon or "") .. mode_name .. (mode.marker or "") .. " ",
      style = { hl_group = spec.hl_group or "SmeltModeDefault" },
      priority = 1,
    }
  end

  -- tok/s.
  local tps = signal("tps") or 0
  if smelt.settings.show_tps and tps > 0 then
    items[#items + 1] = {
      text = string.format("%.1f tok/s", tps),
      style = { fg = "Comment" },
      priority = 4,
      separated = true,
    }
  end

  -- Right-strip indicators.
  if signal("permission_pending") then
    items[#items + 1] = {
      text = "permission pending",
      style = { fg = "SmeltAccent", bold = true },
      priority = 2,
      separated = true,
    }
  end

  local keymap_pending = signal("keymap_pending")
  if keymap_pending and keymap_pending ~= "" then
    items[#items + 1] = {
      text = "key " .. keymap_pending,
      style = { fg = "SmeltAccent", bold = true },
      priority = 2,
      separated = true,
    }
  end

  local procs = signal("running_procs") or 0
  if procs > 0 then
    items[#items + 1] = {
      text = procs == 1 and "1 proc" or (procs .. " procs"),
      style = { fg = "SmeltProcess", italic = false },
      priority = 2,
      separated = true,
    }
  end

  local cwd_worktree_path = signal("cwd_worktree_path")
  if signal("cwd_managed_worktree") and cwd_worktree_path and cwd_worktree_path ~= "" then
    items[#items + 1] = {
      text = cwd_worktree_path,
      style = { fg = "Comment" },
      priority = 7,
      separated = true,
      truncatable = true,
      truncate = "middle",
    }
  end

  local vim_pending = signal("vim_pending_input")
  if vim_pending and vim_pending ~= "" then
    items[#items + 1] = {
      text = vim_pending,
      priority = 2,
      align_right = true,
    }
  end

  local pos = signal("cursor_pos")
  local viewport = signal("viewport_pos")
  if pos and pos.line and pos.line > 0 then
    local scroll_pct = pos.scroll_pct or 0
    if smelt.focus() == "transcript" and viewport and viewport.scroll_pct then
      scroll_pct = viewport.scroll_pct
    end
    items[#items + 1] = {
      text = string.format(
        "%d:%d %d%%",
        pos.line,
        pos.col or 1,
        scroll_pct
      ),
      style = { fg = "Comment" },
      priority = 3,
      align_right = true,
    }
  end

  return items
end

smelt.statusline.add("core", core_compose)

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
    sep_group = "SmeltSeparator",
  })
  bar.write_rows(buf, { row }, NS)
end

-- ── window allocation ───────────────────────────────────────────────

M.win = smelt.win.new(smelt.buf.new({ name = "smelt.statusline" }), {
  name = "smelt.statusline",
  scrollbar = false,
  surface = "selectable_text",
  region = "status",
})

if M.win then
  M.win:set_renderer(render)
  if type(smelt.signal.subscribe) == "function" then
    for _, name in ipairs({
      "agent_mode",
      "cursor_pos",
      "cwd_managed_worktree",
      "cwd_worktree_path",
      "input_epoch",
      "keymap_pending",
      "permission_pending",
      "running_procs",
      "session_epoch",
      "task_label",
      "tps",
      "viewport_pos",
      "vim_mode",
      "vim_pending_input",
    }) do
      renderer_subscriptions[#renderer_subscriptions + 1] = smelt.signal.subscribe(name, M.invalidate)
    end
  end
end

-- Row count this composer paints into. Layout / overlay code that needs
-- to reserve space above the statusline reads this instead of touching
-- the window rect directly (the rect is only valid after the first
-- render, so an overlay opening on cold start would see nil). A plugin
-- replacing this module is free to override `M.rows` if it produces a
-- multi-row statusline.
M.rows = 1

return M
