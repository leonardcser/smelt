-- `smelt.confirm.open(handle_id)` — built-in tool-approval dialog.
--
-- Fired from the agent loop after registering a request in
-- `App::confirm_requests`. Plugins can override this function in
-- their init.lua to swap the default UI.
--
-- Panels (top → bottom, indices below match the keymap callbacks):
--   1 title    — bash-syntax-highlit ` tool: desc Allow?`
--   2 summary  — optional muted summary, hidden when empty
--   3 preview  — diff / notebook / file / bash body, hidden when empty
--   4 options  — yes / no + dynamic "always allow …" entries
--   5 reason   — optional message attached to the decision
--
-- Keys:
--   e                    focus the reason input
--   <S-Tab>              toggle app mode; auto-allow + close when the
--                        new mode covers this request
--   Enter                resolve with the focused option (+ reason text)
--   Esc / Ctrl-C         resolve as "no"
--
-- The preview panel is interactive — click it to focus, then scroll
-- with the wheel or vim motions (j/k, gg/G, Ctrl-D/Ctrl-U).

-- Tools whose preview is rendered into the preview buffer. The Lua
-- side dispatches by tool_name onto the matching renderer primitive.
local function fill_preview(buf, req)
  local tool = req.tool_name
  if tool == "edit_file" then
    smelt.diff.render(buf, {
      old  = req.args.old_string or "",
      new  = req.args.new_string or "",
      path = req.args.file_path or "",
    })
  elseif tool == "write_file" then
    smelt.syntax.render(buf, {
      content = req.args.content or "",
      path    = req.args.file_path or "",
    })
  elseif tool == "edit_notebook" then
    smelt.notebook.render(buf, req.args)
  elseif tool == "bash" and req.desc:find("\n") then
    smelt.bash.render(buf, req.desc)
  end
end

-- `~/`-rewrite of the process cwd, used for "in {cwd}" labels on the
-- workspace-scoped variants. Falls back to the absolute path if the
-- cwd is outside HOME.
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

-- Build (labels, decisions) in parallel from the request payload.
-- Decision strings round-trip through `smelt.confirm._resolve`; the
-- `confirm_resolved` cell payload publishes the same string so plugin
-- subscribers branch on a stable lexicon.
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
  -- Request payload (tool / desc / args / options / approval patterns
  -- / outside_dir / cwd_label / handle_id) flows through the
  -- `confirm_requested` cell. Bail if the cell snapshot doesn't match
  -- this handle (a follow-up request flipped the cell before this
  -- dialog opened — the next `fire_confirm_open` will hand us the
  -- right one).
  local req = smelt.cell("confirm_requested"):get()
  if not req or req.handle_id ~= handle_id then return end

  local title_buf   = smelt.buf.create()
  local summary_buf = smelt.buf.create()
  local preview_buf = smelt.buf.create()

  smelt.confirm._render_title(title_buf, handle_id)
  if req.summary and req.summary ~= "" then
    smelt.buf.set_lines(summary_buf, { " " .. req.summary })
  end
  fill_preview(preview_buf, req)

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
  -- Track the options panel's current selection (list leaves fire
  -- `selection_changed { index = 1-based }` on cursor move) and
  -- whether the reason input has any user-typed text. Both flags
  -- are needed so a Submit from either leaf can resolve with the
  -- right option + the right reason: ctx.index is only present when
  -- Submit comes from the options leaf; the placeholder-vs-typed
  -- distinction can't be made from the buffer alone.
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
    -- "no" is always option 2 (yes/no come first).
    close_with(2, nil)
  end)
end
