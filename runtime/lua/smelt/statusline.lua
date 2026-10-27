-- Statusline window - Lua-allocated, Lua-rendered.
--
-- This module owns:
--   * the statusline window (`M.win`)
--   * a bar (`smelt._bar`, strip layout) holding the named source callbacks
--     registered with `smelt.statusline.add` / `remove`
--   * a retained renderer that asks the bar for one row after semantic
--     invalidation and writes it to the window's buffer.
--
-- The built-in `core` source reads engine state from signals plus
-- `smelt.session.status()` for values that carry pending/stale markers
-- (`vim_mode`, `agent_mode`, `tps`, `task_label`, `running_procs`,
-- `permission_pending`, `keymap_pending`, `vim_pending_input`, `cursor_pos`, `viewport_pos`). Plugins extend the line by
-- registering additional sources via `smelt.statusline.add(name, fn)`.

local bar = require("smelt._bar")

local NS = smelt.ns("smelt.statusline")
local renderer_subscriptions = {}

local line = bar.new({
  name = "smelt.statusline",
  layout = "strip",
  bg_group = "SmeltStatusBg",
  sep_group = "SmeltSeparator",
})

local M = {}
smelt.statusline = M

-- Repaint after plugin-owned state changes that are not represented by a
-- built-in signal. Adding, replacing, and removing sources invalidates
-- automatically.
---@tier ui_host
---@type fun()
function smelt.statusline.invalidate()
  bar.invalidate(line)
end

--- Repaint after `ms` milliseconds. An animated source calls it on every paint
--- to ask for its next frame; the earliest pending request wins.
---@tier ui_host
---@type fun(ms: integer)
function smelt.statusline.invalidate_after(ms)
  bar.invalidate_after(line, ms)
end

--- Add a named source of statusline items. `handler` runs on every repaint and
--- returns a list of items (see `smelt.bar.Item`); errors are reported on
--- stderr and skip the source. Adding a name that already exists replaces that
--- source in place, keeping its position. Returns a `Reg` that removes this
--- source, unless a later `add` with the same name has replaced it.
---@tier ui_host
---@type fun(name: string, handler: fun(): smelt.bar.Item[]?): smelt.Reg
function smelt.statusline.add(name, handler)
  return bar.add(line, name, handler)
end

--- Remove every source named `name`.
---@tier ui_host
---@type fun(name: string)
function smelt.statusline.remove(name)
  bar.remove(line, name)
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

--- The built-in `core` source, so a replacement can wrap it instead of
--- dropping it.
---@tier ui_host
---@type fun(name: string): (fun(): smelt.bar.Item[]?)?
function smelt.statusline.builtin(name)
  if name == "core" then return core_compose end
  return nil
end

smelt.statusline.add("core", core_compose)

-- ── renderer ────────────────────────────────────────────────────────

local function render(win)
  local buf = win:buf()
  if not buf then return end
  bar.write_rows(buf, { bar.row(line, win:content_width() or 80) }, NS)
end

-- ── window allocation ───────────────────────────────────────────────

M.win = smelt.win.new(smelt.buf.new({ name = "smelt.statusline" }), {
  name = "smelt.statusline",
  scrollbar = false,
  surface = "selectable_text",
  region = "status",
})

line.win = M.win
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
