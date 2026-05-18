-- Statusline composer. Builds the `core` source's segments each refresh.
-- Style decisions live in the theme: every segment references a group
-- via `style_group` so `/theme`, `/color`, and user overrides cascade
-- without status.lua having to read or project palette colors.

local M = {}

local function compose()
  local snap = smelt.statusline.snapshot()
  if not snap then return {} end

  local working = snap.working or {}
  local items = {}

  -- ── Slug / compacting pill ─────────────────────────────────────────
  local compacting = working.busy_label == "compacting"
  local live = working.animating
  local label
  if live then
    if compacting then
      label = "compacting"
    elseif snap.settings and snap.settings.show_slug then
      label = snap.task_label or "working"
    else
      label = "working"
    end
  elseif snap.settings and snap.settings.show_slug then
    label = snap.task_label
  end

  -- SmeltSlug carries the pill fg only; the bg defaults to SmeltAccent
  -- unless `/color` has set an explicit slug bg. Resolved here, not in
  -- the engine, so the cascade rule stays out of Rust.
  local slug_extra
  if not compacting then
    local slug = smelt.theme.get("SmeltSlug") or {}
    if not slug.bg then
      local accent = smelt.theme.get("SmeltAccent") or {}
      if accent.fg then slug_extra = { bg = accent.fg } end
    end
  end

  local pill_group = compacting and "SmeltCompacting" or "SmeltSlug"
  if working.spinner_char then
    table.insert(items, {
      text = " " .. working.spinner_char,
      style_group = pill_group,
      style = slug_extra,
      priority = 0,
    })
  end
  if label then
    table.insert(items, {
      text = " " .. label .. " ",
      style_group = pill_group,
      style = slug_extra,
      priority = 5,
      truncatable = true,
    })
  end

  -- ── Vim mode pill ──────────────────────────────────────────────────
  if snap.vim and snap.vim.enabled then
    local vim_group
    if snap.vim.kind == "insert" then vim_group = "SmeltVimInsert"
    elseif snap.vim.kind == "visual" then vim_group = "SmeltVimVisual"
    else vim_group = "SmeltVimNormal" end
    table.insert(items, {
      text = " " .. (snap.vim.label or "NORMAL") .. " ",
      style_group = vim_group,
      priority = 3,
    })
  end

  -- ── Agent mode pill ────────────────────────────────────────────────
  local mode = snap.mode
  if mode then
    local mode_group
    if mode.name == "plan" then mode_group = "SmeltModePlan"
    elseif mode.name == "apply" then mode_group = "SmeltModeApply"
    elseif mode.name == "yolo" then mode_group = "SmeltModeYolo"
    elseif mode.name == "exec" then mode_group = "SmeltModeExec"
    else mode_group = "SmeltModeDefault" end
    local icon = smelt.mode.icon and smelt.mode.icon(mode.name) or ""
    table.insert(items, {
      text = " " .. icon .. (mode.name or "") .. " ",
      style_group = mode_group,
      priority = 1,
    })
  end

  -- ── Throbber: skip the first span when animating (slug pill already shows the spinner). ──
  local throb = working.throbber or {}
  local skip = (working.animating and #throb > 0) and 1 or 0
  for i = skip + 1, #throb do
    local span = throb[i]
    local prio = span.priority or 0
    if prio == 0 then prio = 4
    elseif prio == 3 then prio = 6 end
    table.insert(items, {
      text = span.text,
      style_group = span.muted and "Comment" or nil,
      style = { bold = span.bold, dim = span.dim },
      priority = prio,
    })
  end

  -- ── Right-strip indicators ────────────────────────────────────────
  if snap.permission_pending then
    table.insert(items, {
      text = "permission pending",
      style_group = "SmeltAccent",
      style = { bold = true },
      priority = 2,
      separated = true,
    })
  end

  local procs = snap.running_procs or 0
  if procs > 0 then
    table.insert(items, {
      text = procs == 1 and "1 proc" or (procs .. " procs"),
      style_group = "SmeltAccent",
      priority = 2,
      separated = true,
    })
  end
  local agents = snap.running_agents or 0
  if agents > 0 then
    table.insert(items, {
      text = agents == 1 and "1 agent" or (agents .. " agents"),
      style_group = "SmeltAccent",
      priority = 2,
      separated = true,
    })
  end

  if snap.position and snap.position.text then
    table.insert(items, {
      text = snap.position.text,
      style_group = "Comment",
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
