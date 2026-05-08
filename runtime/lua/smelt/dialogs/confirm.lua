-- Built-in tool-approval dialog. Override `smelt.confirm.open` in init.lua to
-- swap the default UI. Tool `preview` callbacks live in each tool's Lua definition.

-- `~/`-rewrite of the process cwd for workspace-scoped "always allow" labels.
local function pretty_cwd()
  local cwd = smelt.os.cwd() or ""
  local home = smelt.os.home()
  if home and home ~= "" and cwd:sub(1, #home) == home then
    local rest = cwd:sub(#home + 1)
    if rest == "" then return "~" end
    return "~" .. rest
  end
  return cwd
end

-- Build option labels and decision strings from the request payload.
local function build_options(req)
  local labels, decisions = {}, {}
  local function push(label, decision)
    labels[#labels + 1] = label
    decisions[#decisions + 1] = decision
  end

  push("yes", "yes")
  push("no", "no")

  local cwd = pretty_cwd()
  local has_dir = req.outside_dir ~= nil and req.outside_dir ~= ""
  local has_patterns = req.approval_patterns and #req.approval_patterns > 0

  if has_dir then
    local dir = req.outside_dir
    push("allow " .. dir, "always_dir_session")
    push("allow " .. dir .. " in " .. cwd, "always_dir_workspace")
  end
  if has_patterns then
    local display = {}
    for i, p in ipairs(req.approval_patterns) do
      local d = p:gsub("/%*$", "")
      local stripped = d:match("^[^:]+://(.+)$") or d
      display[i] = stripped
    end
    local display_str = table.concat(display, ", ")
    push("allow " .. display_str, "always_pattern_session")
    push("allow " .. display_str .. " in " .. cwd, "always_pattern_workspace")
  end
  if not has_dir and not has_patterns then
    push("always allow", "always_session")
    push("always allow in " .. cwd, "always_workspace")
  end

  return labels, decisions
end

function smelt.confirm.open(handle_id)
  -- Bail if the cell doesn't match this handle; a newer request may have
  -- replaced it before this dialog opened.
  local req = smelt.cell("confirm_requested"):get()
  if not req or req.handle_id ~= handle_id then return end

  local title_buf   = smelt.buf.create()
  local summary_buf = smelt.buf.create()
  local preview_buf = smelt.buf.create()

  smelt.confirm._render_title(title_buf, handle_id)
  if req.summary and req.summary ~= "" then
    smelt.buf.set_lines(summary_buf, { " " .. req.summary })
  end
  smelt.confirm._render_preview(preview_buf, handle_id)

  local labels, decisions = build_options(req)
  local items = {}
  for i, label in ipairs(labels) do
    items[i] = { label = label }
  end

  local panels = {
    { kind = "content", buf = title_buf,   height = "fit",  focusable = false, name = "title"   },
    { kind = "content", buf = summary_buf, height = "fit",  focusable = false, collapse_when_empty = true, name = "summary" },
    {
      kind                = "content",
      buf                 = preview_buf,
      height              = "fill",
      interactive         = true,
      collapse_when_empty = true,
      separator           = "dashed",
      name                = "preview",
    },
    { kind = "options", items = items, focus = true, name = "options" },
    { kind = "input", placeholder = "reason (optional)…", collapse_when_empty = true, name = "reason" },
  }

  local d = smelt.ui.dialog.open_handle({
    panels           = panels,
    blocks_agent     = true,
    placement        = "dock_bottom",
    placement_height = 100,
  })
  if not d then return end

  local resolved = false
  local selected_idx = 1
  local typed_reason = false
  local function close_with(idx, message)
    if resolved then return end
    resolved = true
    local decision = decisions[idx] or "no"
    smelt.confirm._resolve(handle_id, decision, message)
    d:close()
  end

  smelt.win.set_keymap(d.win, "e",         function() d.panels.reason:focus()      end)
  smelt.win.set_keymap(d.win, "s-tab",     function()
    if smelt.confirm._back_tab(handle_id) then
      resolved = true
      d:close()
    end
  end)

  smelt.win.on_event(d.win, "selection_changed", function(ctx)
    if ctx.index then selected_idx = ctx.index end
  end)

  smelt.win.on_event(d.win, "text_changed", function()
    typed_reason = true
  end)

  smelt.win.on_event(d.win, "submit", function(ctx)
    local idx = ctx.index or selected_idx
    local message = nil
    if typed_reason and d.panels.reason then
      message = d.panels.reason:text()
      if message == "" then message = nil end
    end
    close_with(idx, message)
  end)

  smelt.win.on_event(d.win, "dismiss", function()
    close_with(2, nil) -- "no" is always option 2
  end)
end
