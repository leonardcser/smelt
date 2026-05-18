-- Statusline composer. Registers a `core` source returning segments for the
-- Rust layout engine (priority, truncation, alignment).

local M = {}

local STATUS_BG = 233
local PILL_FG = 0
local COMPACTING_BG = 15
local VIM_BG = 236
local VIM_INSERT_FG = 78
local VIM_VISUAL_FG = 176
local VIM_NORMAL_FG = 74
local MODE_BG = 234

local function vim_fg(kind)
  if kind == "insert" then return VIM_INSERT_FG end
  if kind == "visual" then return VIM_VISUAL_FG end
  return VIM_NORMAL_FG
end

local function compose()
  local snap = smelt.statusline.snapshot()
  if not snap or not snap.theme then return {} end

  local theme = snap.theme or {}
  local working = snap.working or {}
  local items = {}

  local pill_bg = working.compacting and COMPACTING_BG or theme.slug_bg or theme.accent_fg
  local live = working.animating
  local label
  if live then
    if working.compacting then
      label = "compacting"
    elseif snap.settings and snap.settings.show_slug then
      label = snap.task_label or "working"
    else
      label = "working"
    end
  elseif snap.settings and snap.settings.show_slug then
    label = snap.task_label
  end

  if working.spinner_char then
    table.insert(items, {
      text = " " .. working.spinner_char,
      fg = PILL_FG,
      bg = pill_bg,
      priority = 0,
    })
  end
  if label then
    table.insert(items, {
      text = " " .. label .. " ",
      fg = PILL_FG,
      bg = pill_bg,
      priority = 5,
      truncatable = true,
    })
  end

  if snap.vim and snap.vim.enabled then
    table.insert(items, {
      text = " " .. (snap.vim.label or "NORMAL") .. " ",
      fg = vim_fg(snap.vim.kind),
      bg = VIM_BG,
      priority = 3,
    })
  end

  local mode = snap.mode
  if mode then
    local mode_fg
    if mode.name == "plan" then
      mode_fg = theme.plan_fg
    elseif mode.name == "apply" then
      mode_fg = theme.apply_fg
    elseif mode.name == "yolo" then
      mode_fg = theme.yolo_fg
    else
      mode_fg = theme.muted_fg
    end
    local icon = smelt.mode.icon and smelt.mode.icon(mode.name) or ""
    table.insert(items, {
      text = " " .. icon .. (mode.name or "") .. " ",
      fg = mode_fg,
      bg = MODE_BG,
      priority = 1,
    })
  end

  -- Throbber: skip the first span when animating (slug pill already shows the spinner).
  local throb = working.throbber or {}
  local skip = (working.animating and #throb > 0) and 1 or 0
  for i = skip + 1, #throb do
    local span = throb[i]
    local prio = span.priority or 0
    if prio == 0 then prio = 4
    elseif prio == 3 then prio = 6
    end
    table.insert(items, {
      text = span.text,
      fg = span.fg,
      bg = STATUS_BG,
      bold = span.bold,
      priority = prio,
    })
  end

  if snap.permission_pending then
    table.insert(items, {
      text = "permission pending",
      fg = theme.accent_fg,
      bg = STATUS_BG,
      bold = true,
      priority = 2,
      group = true,
    })
  end

  local procs = snap.running_procs or 0
  if procs > 0 then
    table.insert(items, {
      text = procs == 1 and "1 proc" or (procs .. " procs"),
      fg = theme.accent_fg,
      bg = STATUS_BG,
      priority = 2,
      group = true,
    })
  end
  local agents = snap.running_agents or 0
  if agents > 0 then
    table.insert(items, {
      text = agents == 1 and "1 agent" or (agents .. " agents"),
      fg = theme.agent_fg,
      bg = STATUS_BG,
      priority = 2,
      group = true,
    })
  end

  if snap.position and snap.position.text then
    table.insert(items, {
      text = snap.position.text,
      fg = theme.muted_fg,
      bg = STATUS_BG,
      priority = 3,
      align_right = true,
    })
  end

  return items
end

function M.setup()
  smelt.statusline.register("core", compose)
end

M.setup()

return M
