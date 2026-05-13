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

  -- Title / summary / preview content buffers (consumer-built).
  local title_buf   = smelt.buf.create()
  local summary_buf = smelt.buf.create()
  local preview_buf = smelt.buf.create()
  smelt.confirm._render_title(title_buf, handle_id)
  if req.summary and req.summary ~= "" then
    smelt.buf.set_lines(summary_buf, { " " .. req.summary })
  end
  smelt.confirm._render_preview(preview_buf, handle_id)

  local labels, decisions = build_options(req)

  local title_leaf   = smelt.ui.dialog.content({ buf = title_buf })
  local summary_leaf = smelt.ui.dialog.content({ buf = summary_buf })
  local preview_leaf = smelt.ui.dialog.content({ buf = preview_buf, interactive = true })
  local options_leaf = smelt.ui.dialog.options(labels)
  local reason_leaf, reason_buf = smelt.ui.dialog.input("reason (optional)…")

  local typed_reason = false
  smelt.win.on_event(reason_leaf, "text_changed", function() typed_reason = true end)

  local resolved = false
  local function close_with(idx, message)
    if resolved then return end
    resolved = true
    smelt.confirm._resolve(handle_id, decisions[idx] or "no", message)
  end

  local handle = smelt.ui.dialog.open_handle({
    blocks_agent = true,
    height       = 100,
    panels = {
      { leaf = title_leaf,   height = "fit"                                       },
      { leaf = summary_leaf, height = "fit",  collapse_when_empty = true          },
      { leaf = preview_leaf, height = "fill", collapse_when_empty = true          },
      { leaf = options_leaf                                                        },
      { leaf = reason_leaf,                   collapse_when_empty = true          },
    },
    focus = options_leaf,
    keymaps = {
      { key = "s-tab", on_press = function(ctx)
          if smelt.confirm._back_tab(handle_id) then
            resolved = true
            ctx.close()
          end
        end },
    },
    on_submit = function(ctx)
      local idx = (smelt.win.cursor_row(options_leaf) or 0) + 1
      local message = nil
      if typed_reason then
        local line = smelt.buf.get_line(reason_buf, 1) or ""
        if line ~= "" then message = line end
      end
      close_with(idx, message)
      ctx.close()
    end,
    on_dismiss = function(ctx)
      close_with(2, nil) -- "no" is always option 2
      ctx.close()
    end,
  })

  -- `e` only fires when the options panel has focus: it switches focus to the
  -- reason input. Installing on the options leaf scopes it correctly so typing
  -- `e` while editing the reason still types a literal `e`.
  smelt.win.set_keymap(options_leaf, "e", function()
    smelt.win.set_focus(reason_leaf)
  end)

  return handle
end
