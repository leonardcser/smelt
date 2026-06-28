-- Statusline window - Lua-allocated, Lua-rendered.
--
-- This module owns:
--   * the statusline window (`M.win`)
--   * a registry of named source callbacks (`M.add` / `M.remove`)
--   * a per-frame renderer that calls each source, flattens the
--     returned segments, composes via `_bar.compose_status`, and
--     writes to the window's buffer.
--
-- The built-in `core` source reads engine state from signals plus
-- `smelt.session.status()` for values that carry pending/stale markers
-- (`vim_mode`, `agent_mode`, `tps`, `task_label`, `running_procs`,
-- `permission_pending`, `keymap_pending`, `vim_pending_input`, `cursor_pos`, `viewport_pos`). Plugins extend the line by
-- registering additional sources via `M.add(name, fn)`.

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

function M.remove(name)
  for i = #sources, 1, -1 do
    if sources[i].name == name then table.remove(sources, i) end
  end
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
    local icon = smelt.mode.icon and smelt.mode.icon(mode_name) or ""
    items[#items + 1] = {
      text = " " .. icon .. mode_name .. (mode.marker or "") .. " ",
      style = smelt.mode.style and smelt.mode.style(mode_name) or { hl_group = "SmeltModeDefault" },
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
    sep_group = "SmeltBar",
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

if M.win then M.win:set_renderer(render) end

-- Row count this composer paints into. Layout / overlay code that needs
-- to reserve space above the statusline reads this instead of touching
-- the window rect directly (the rect is only valid after the first
-- render, so an overlay opening on cold start would see nil). A plugin
-- replacing this module is free to override `M.rows` if it produces a
-- multi-row statusline.
M.rows = 1

return M
