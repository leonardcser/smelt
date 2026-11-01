-- The statusline: a strip bar at the bottom of the screen.
--
-- The built-in sources (`slug`, `vim_mode`, `agent_mode`, `tps`,
-- `permission`, `keymap`, `procs`, `worktree`, `vim_pending`, `cursor`) read
-- engine state from signals plus `smelt.session.status()` for values that
-- carry pending/stale markers. Plugins add sources with
-- `smelt.statusline:add(name, fn)`, and replace or remove a built-in one by
-- its name.

local bar = require("smelt._bar")

-- ── built-in sources ────────────────────────────────────────────────

local function vim_group(label)
  if label == "INSERT" then return "SmeltVimInsert"
  elseif label == "VISUAL" or label == "V-LINE" then return "SmeltVimVisual"
  else return "SmeltVimNormal"
  end
end

local function signal(name) return smelt.signal.get(name) end

local sources = {}

local function define(name, handler)
  sources[#sources + 1] = { name, handler }
end

-- Slug pill. SmeltSlug carries fg only; fall back to SmeltAccent's fg as the
-- pill bg when no explicit bg has been set (default), so `/color` (which
-- writes SmeltSlug.bg) and theme swaps both propagate naturally.
define("slug", function()
  local task_label = signal("task_label")
  if not smelt.settings.show_slug or not task_label or task_label == "" then return nil end
  local slug = smelt.theme.get("SmeltSlug") or {}
  local style = { hl_group = "SmeltSlug" }
  if not slug.bg then
    local accent = smelt.theme.get("SmeltAccent")
    if accent and accent.fg then
      style.bg = accent.fg
    end
  end
  return {
    text = " " .. task_label .. " ",
    style = style,
    priority = 5,
    truncatable = true,
  }
end)

define("vim_mode", function()
  local vim_label = signal("vim_mode")
  if not vim_label or vim_label == "" then return nil end
  return {
    text = " " .. vim_label .. " ",
    style = { hl_group = vim_group(vim_label) },
    priority = 3,
  }
end)

define("agent_mode", function()
  local status = smelt.session.status and smelt.session.status() or {}
  local mode = status.mode or {}
  local mode_name = mode.name or signal("agent_mode")
  if not mode_name or mode_name == "" then return nil end
  local spec = smelt.mode.get(mode_name) or {}
  return {
    text = " " .. (spec.icon or "") .. mode_name .. (mode.marker or "") .. " ",
    style = { hl_group = spec.hl_group or "SmeltModeDefault" },
    priority = 1,
  }
end)

define("tps", function()
  local tps = signal("tps") or 0
  if not smelt.settings.show_tps or tps <= 0 then return nil end
  return {
    text = string.format("%.1f tok/s", tps),
    style = { fg = "Comment" },
    priority = 4,
    separated = true,
  }
end)

define("permission", function()
  if not signal("permission_pending") then return nil end
  return {
    text = "permission pending",
    style = { fg = "SmeltAccent", bold = true },
    priority = 2,
    separated = true,
  }
end)

define("keymap", function()
  local keymap_pending = signal("keymap_pending")
  if not keymap_pending or keymap_pending == "" then return nil end
  return {
    text = "key " .. keymap_pending,
    style = { fg = "SmeltAccent", bold = true },
    priority = 2,
    separated = true,
  }
end)

define("procs", function()
  local procs = signal("running_procs") or 0
  if procs <= 0 then return nil end
  return {
    text = procs == 1 and "1 proc" or (procs .. " procs"),
    style = { fg = "SmeltProcess", italic = false },
    priority = 2,
    separated = true,
  }
end)

define("worktree", function()
  local cwd_worktree_path = signal("cwd_worktree_path")
  if not signal("cwd_managed_worktree") or not cwd_worktree_path or cwd_worktree_path == "" then
    return nil
  end
  return {
    text = cwd_worktree_path,
    style = { fg = "Comment" },
    priority = 7,
    separated = true,
    truncatable = true,
    truncate = "middle",
  }
end)

define("vim_pending", function()
  local vim_pending = signal("vim_pending_input")
  if not vim_pending or vim_pending == "" then return nil end
  return {
    text = vim_pending,
    priority = 2,
    align_right = true,
  }
end)

define("cursor", function()
  local pos = signal("cursor_pos")
  if not pos or not pos.line or pos.line <= 0 then return nil end
  local viewport = signal("viewport_pos")
  local scroll_pct = pos.scroll_pct or 0
  if smelt.focus() == "transcript" and viewport and viewport.scroll_pct then
    scroll_pct = viewport.scroll_pct
  end
  return {
    text = string.format("%d:%d %d%%", pos.line, pos.col or 1, scroll_pct),
    style = { fg = "Comment" },
    priority = 3,
    align_right = true,
  }
end)

--- The strip at the bottom of the screen, a `smelt.bar.Bar`. Built-in
--- sources: `slug`, `vim_mode`, `agent_mode`, `tps`, `permission`, `keymap`,
--- `procs`, `worktree`, `vim_pending` and `cursor`. Items with `row = 2` or
--- more make it taller.
---@tier ui_host
---@type smelt.bar.Bar
smelt.statusline = bar.new({
  name = "smelt.statusline",
  layout = "strip",
  bg = "SmeltStatusBg",
  separator_style = { fg = "SmeltSeparator", dim = true },
  sources = sources,
})

local function invalidate() smelt.statusline:invalidate() end
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
  smelt.signal.subscribe(name, invalidate)
end

return smelt.statusline
