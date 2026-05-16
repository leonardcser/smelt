-- Built-in tool-approval dialog. Override `smelt.confirm.open` in init.lua to
-- swap the default UI. Tool `preview` callbacks live in each tool's Lua definition.

local NS_NUM = smelt.buf.create_namespace("smelt.confirm.num")
local NS_SEL = smelt.buf.create_namespace("smelt.confirm.sel")

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

-- Compose the body header: the tool's `summary(args)` output (already a
-- styled-lines payload — each span carries its own syntax/style) plus an
-- optional subtitle. The tool name itself lives in the overlay border title;
-- "Allow?" lives in its own panel below the preview.
--
-- Subtitle convention: if the tool's arguments include a non-empty
-- `description` string, surface it dimmed under the body. Tools opt in by
-- declaring `description` in their parameter schema (bash does this to carry
-- the model's short gloss of what the command does); no other wiring needed.
local function render_header(buf, req)
  local subtitle = nil
  local desc = req.args and req.args.description
  if type(desc) == "string" and desc ~= "" then
    subtitle = desc
  end

  local lines = {}
  if req.summary then
    for _, line in ipairs(req.summary) do
      lines[#lines + 1] = line
    end
  end
  if #lines == 0 then
    lines[#lines + 1] = { { text = "" } }
  end
  if subtitle then
    lines[#lines + 1] = { { text = subtitle, style = { dim = true } } }
  end
  smelt.buf.set_styled_lines(buf, lines)
end

-- Paint " N. " dim numbering prefixes on each option row and stamp a cursor-row-only
-- accent extmark over the label so the selected option's label flips to SmeltAccent.
local function render_options(buf, labels)
  local rendered = {}
  local label_starts = {}
  for i, label in ipairs(labels) do
    local prefix = string.format(" %d. ", i)
    rendered[i] = prefix .. label
    label_starts[i] = #prefix
  end
  smelt.buf.set_lines(buf, rendered)
  smelt.buf.clear_namespace(buf, NS_NUM)
  smelt.buf.clear_namespace(buf, NS_SEL)
  for i, start in ipairs(label_starts) do
    smelt.buf.set_extmark(buf, NS_NUM, i, 0, { end_col = start, dim = true })
    smelt.buf.set_extmark(buf, NS_SEL, i, start, {
      end_col       = #rendered[i],
      hl_group      = "SmeltAccent",
      on_cursor_row = true,
    })
  end
end

function smelt.confirm.open(handle_id)
  -- Bail if the cell doesn't match this handle; a newer request may have
  -- replaced it before this dialog opened.
  local req = smelt.cell("confirm_requested"):get()
  if not req or req.handle_id ~= handle_id then return end

  local header_buf  = smelt.buf.create()
  local preview_buf = smelt.buf.create()
  render_header(header_buf, req)
  smelt.confirm._render_preview(preview_buf, handle_id)

  local labels, decisions = build_options(req)

  local header_leaf  = smelt.ui.dialog.content({ buf = header_buf, wrap = false })
  local preview_leaf = smelt.ui.dialog.content({ buf = preview_buf, interactive = true })
  local allow_leaf, allow_buf = smelt.ui.dialog.content({ wrap = false })
  smelt.buf.set_styled_lines(allow_buf, {
    { { text = "Allow?", style = { dim = true } } },
  })
  local options_leaf, options_buf = smelt.ui.dialog.options(labels)
  render_options(options_buf, labels)
  local reason_leaf, reason_buf =
      smelt.ui.dialog.input("press tab to add a reason…", { pad_left = 2 })

  -- Empty 1-row spacer panel that visually separates the options list from
  -- the reason input.
  local spacer_leaf = smelt.ui.dialog.content({ text = "", wrap = false })

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
    max_height   = "fill",
    title        = req.tool_name,
    panels = {
      { leaf = header_leaf,  height = "fit"                              },
      { leaf = preview_leaf, height = "fit", collapse_when_empty = true,
        border = { style = "dashed", top = "Comment", bottom = "Comment" } },
      { leaf = allow_leaf,   height = "fit"                              },
      { leaf = options_leaf, height = "fit"                              },
      { leaf = spacer_leaf,  height = "fit"                              },
      { leaf = reason_leaf,                  collapse_when_empty = true  },
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

  -- Tab from the options leaf jumps focus into the reason input; Esc inside
  -- the reason input pops focus back to the options leaf (instead of
  -- dismissing the dialog — that still works from the options leaf). Scoping
  -- both keymaps per-leaf means typing literal Tab/Esc characters in the
  -- input would only ever do the configured action.
  smelt.win.set_keymap(options_leaf, "tab", function()
    smelt.win.set_focus(reason_leaf)
  end)
  smelt.win.set_keymap(reason_leaf, "esc", function()
    smelt.win.set_focus(options_leaf)
  end)

  return handle
end
