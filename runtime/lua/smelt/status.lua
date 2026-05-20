-- Statusline composer. Builds the `core` source's segments each refresh.
-- Style decisions live in the theme: every segment references a group
-- via `style_group` so `/theme`, `/color`, and user overrides cascade
-- without status.lua having to read or project palette colors.

local M = {}

local function compose()
  local snap = smelt.statusline.snapshot()
  if not snap then return {} end

  local items = {}

  -- ── Slug pill (always shows when show_slug is on; working state lives in the prompt top bar) ──
  if snap.settings and snap.settings.show_slug and snap.task_label then
    local slug = smelt.theme.get("SmeltSlug") or {}
    local slug_extra
    if not slug.bg then
      local accent = smelt.theme.get("SmeltAccent") or {}
      if accent.fg then slug_extra = { bg = accent.fg } end
    end
    table.insert(items, {
      text = " " .. snap.task_label .. " ",
      style_group = "SmeltSlug",
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

  -- ── Tokens-per-second (right strip) ───────────────────────────────
  if snap.settings and snap.settings.show_tps and snap.tps then
    table.insert(items, {
      text = string.format(" %.1f tok/s", snap.tps),
      style_group = "Comment",
      priority = 4,
    })
  end

  -- ── Cache hit ratio (right strip) ────────────────────────────────
  -- Only emit once any data exists; `cache_hit_ratio` is nil until the
  -- first prompt is observed. Grouped with the token indicators so it
  -- hides when `show_tokens` is off.
  if snap.settings and snap.settings.show_tokens then
    local tokens = smelt.session.tokens()
    if tokens and tokens.cache_hit_ratio then
      local pct = math.floor(tokens.cache_hit_ratio * 100 + 0.5)
      table.insert(items, {
        text = "cache " .. pct .. "%",
        style_group = "Comment",
        priority = 4,
        separated = true,
      })
    end
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
