-- Built-in tool-approval dialog. Override `smelt.confirm.open` in init.lua to
-- swap the default UI. Tool `preview` callbacks live in each tool's Lua definition.

local NS_NUM = smelt.ns("smelt.confirm.num")
local NS_SEL = smelt.ns("smelt.confirm.sel")

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

-- Compose the body header: `tool_name: ` (name in SmeltAccent) followed by
-- the tool's `summary(args)` output. Continuation lines of a multi-line
-- summary are indented to align under the first character after the colon.
-- An optional dim subtitle from `args.description` follows.
local function render_header(buf, req)
  local tool_name = req.tool_name or ""
  local indent = string.rep(" ", #tool_name + 2)

  local lines = {}
  local summary_lines = req.summary or {}
  if #summary_lines == 0 then
    lines[#lines + 1] = {
      { text = tool_name, style = { hl = "SmeltAccent" } },
      { text = ":" },
    }
  else
    for i, line in ipairs(summary_lines) do
      local new_line
      if i == 1 then
        new_line = {
          { text = tool_name, style = { hl = "SmeltAccent" } },
          { text = ": " },
        }
      else
        new_line = { { text = indent } }
      end
      for _, span in ipairs(line) do
        new_line[#new_line + 1] = span
      end
      lines[#lines + 1] = new_line
    end
  end

  local desc = req.args and req.args.description
  if type(desc) == "string" and desc ~= "" then
    lines[#lines + 1] = { { text = desc, style = { dim = true } } }
  end

  buf:styled(lines)
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
  buf:lines(rendered):clear_ns(NS_NUM):clear_ns(NS_SEL)
  for i, start in ipairs(label_starts) do
    buf:mark(NS_NUM, i, 0, { end_col = start, dim = true })
    buf:mark(NS_SEL, i, start, {
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

  local header_buf  = smelt.buf.new()
  local preview_buf = smelt.buf.new()
  render_header(header_buf, req)
  smelt.confirm._render_preview(preview_buf, handle_id)
  local first_preview = preview_buf:line(1)
  local has_preview = first_preview ~= nil and first_preview ~= ""

  local labels, decisions = build_options(req)

  local header_leaf  = smelt.ui.dialog.content({ buf = header_buf, wrap = false })
  local preview_leaf = smelt.ui.dialog.content({ buf = preview_buf, interactive = true })
  local allow_leaf, allow_buf = smelt.ui.dialog.content({ wrap = false })
  local allow_lines = {}
  if not has_preview then
    allow_lines[#allow_lines + 1] = {}
  end
  allow_lines[#allow_lines + 1] = { { text = "Allow?", style = { dim = true } } }
  allow_buf:styled(allow_lines)
  local options_leaf, options_buf = smelt.ui.dialog.options(labels)
  render_options(options_buf, labels)
  local reason_leaf, reason_buf =
      smelt.ui.dialog.input("press tab to add a reason…", { pad_left = 2 })

  -- Empty 1-row spacer panel that visually separates the options list from
  -- the reason input.
  local spacer_leaf = smelt.ui.dialog.content({ text = "", wrap = false })

  local typed_reason = false
  reason_leaf:on("text_changed", function() typed_reason = true end)

  local resolved = false
  local function close_with(idx, message)
    if resolved then return end
    resolved = true
    smelt.confirm._resolve(handle_id, decisions[idx] or "no", message)
  end

  local handle = smelt.ui.dialog.open_handle({
    blocks_agent = true,
    max_height   = "fill",
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
      local idx = (options_leaf:cursor() or 0) + 1
      local message = nil
      if typed_reason then
        local line = reason_buf:line(1) or ""
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
  options_leaf:key("tab", function()
    reason_leaf:focus()
  end)
  reason_leaf:key("esc", function()
    options_leaf:focus()
  end)

  return handle
end
