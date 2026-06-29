-- Centered modal overlay with horizontal action buttons.

local M = {}

local NS_ACTION_LABEL = smelt.ns("smelt.modal.action.label")
local NS_ACTION_DISABLED = smelt.ns("smelt.modal.action.disabled")

local function normalize_lines(lines)
  if type(lines) == "string" then return { { { text = lines } } } end
  if type(lines) ~= "table" then return {} end
  local out = {}
  for i, line in ipairs(lines) do
    if type(line) == "string" then
      out[i] = { { text = line } }
    else
      out[i] = line
    end
  end
  return out
end

local function normalize_actions(actions)
  local out = {}
  for i, action in ipairs(actions or {}) do
    if type(action) == "string" then
      out[i] = { label = action }
    elseif type(action) == "table" then
      local copy = {}
      for k, v in pairs(action) do copy[k] = v end
      copy.label = copy.label or ""
      out[i] = copy
    end
  end
  if #out == 0 then out[1] = { label = "close", value = "close" } end
  return out
end

local function render_actions(buf, actions, selected, width)
  local parts = {}
  local spans = {}
  local total = 0
  for i, action in ipairs(actions) do
    local label = " " .. tostring(action.label or "") .. " "
    parts[i] = label
    total = total + #label
  end
  local gap = #actions > 1 and 3 or 0
  total = total + gap * math.max(#actions - 1, 0)
  local pad = math.max(0, math.floor(((tonumber(width) or total) - total) / 2))
  local line = string.rep(" ", pad)
  local col = pad
  for i, label in ipairs(parts) do
    if i > 1 then
      line = line .. string.rep(" ", gap)
      col = col + gap
    end
    local start_col = col
    line = line .. label
    col = col + #label
    spans[i] = { start_col = start_col, end_col = col, disabled = actions[i].disabled == true }
  end

  buf:lines({ line }):clear_ns(NS_ACTION_LABEL):clear_ns(NS_ACTION_DISABLED)
  for i, item in ipairs(spans) do
    if item.disabled then
      buf:mark(NS_ACTION_DISABLED, 1, item.start_col, { end_col = item.end_col, dim = true })
    else
      local mark = {
        end_col = item.end_col,
        fg = "Normal",
        bg = "SmeltCursorLineBg",
      }
      if i == selected then
        mark.reverse = true
        mark.bold = true
      end
      buf:mark(NS_ACTION_LABEL, 1, item.start_col, mark)
    end
  end
end

local function modal_title(title)
  if title == nil or title == "" then return nil end
  if smelt.dialog and smelt.dialog.title then return smelt.dialog.title(title, { pad = true }) end
  if type(title) == "table" then return title end
  return { { text = " " .. tostring(title) .. " ", dim = true } }
end

function M.open(opts)
  opts = opts or {}
  local actions = normalize_actions(opts.actions)
  local body_buf = smelt.buf.new({ readonly = true })
  body_buf:styled(normalize_lines(opts.lines or opts.body or ""))

  local actions_buf = smelt.buf.new()
  local selected = tonumber(opts.selected or 1) or 1
  if selected < 1 then selected = 1 end
  if selected > #actions then selected = #actions end
  local action_width = opts.action_width or ((type(opts.width) == "number" and opts.width - 4) or 52)
  render_actions(actions_buf, actions, selected, action_width)

  local body = smelt.win.new(body_buf, {
    region = "modal_overlay",
    surface = "readonly_text",
    wrap = true,
    hide_cursor = true,
    scrollbar = false,
  })
  local action_leaf = smelt.win.new(actions_buf, {
    region = "modal_overlay",
    surface = "readonly_text",
    hide_cursor = true,
    scrollbar = false,
  })
  local spacer_buf = smelt.buf.new({ readonly = true })
  spacer_buf:lines({ "" })
  local spacer = smelt.win.new(spacer_buf, {
    region = "modal_overlay",
    surface = "readonly_text",
    hide_cursor = true,
    scrollbar = false,
  })

  local closed = false
  local handle = {}

  local function close()
    if closed then return end
    closed = true
    if handle.overlay then handle.overlay:close() end
    if body then body:close() end
    if spacer then spacer:close() end
    if action_leaf then action_leaf:close() end
    if type(opts.on_close) == "function" then pcall(opts.on_close) end
  end

  local function select_action(index)
    if index < 1 then index = #actions end
    if index > #actions then index = 1 end
    selected = index
    render_actions(actions_buf, actions, selected, action_width)
  end

  local function submit(index)
    if closed then return end
    index = index or selected
    local action = actions[index]
    if not action or action.disabled then return end
    close()
    if type(opts.on_submit) == "function" then
      local ok, err = pcall(opts.on_submit, action.value or action.id or index, action, index)
      if not ok then smelt.notify.error("modal submit: " .. tostring(err)) end
    end
  end

  action_leaf:key("enter", function() submit() end)
  action_leaf:key("left", function() select_action(selected - 1) end)
  action_leaf:key("right", function() select_action(selected + 1) end)
  action_leaf:key("tab", function() select_action(selected + 1) end)
  action_leaf:key("s-tab", function() select_action(selected - 1) end)
  action_leaf:key("esc", close)
  if opts.close_with_q ~= false then action_leaf:key("q", close) end
  action_leaf:key("c-c", close)
  action_leaf:on("dismiss", close)

  local layout = smelt.ui.layout.vbox({
    { smelt.ui.layout.leaf(body), height = "fit" },
    { smelt.ui.layout.leaf(spacer), height = 1 },
    { smelt.ui.layout.leaf(action_leaf), height = 1 },
  }, { padding = 1 })

  handle.overlay = smelt.overlay.new({
    title = modal_title(opts.title),
    anchor = "center",
    border = opts.border or { all = "Normal" },
    modal = true,
    blocks_agent = opts.blocks_agent or false,
    draggable = false,
    resizable = false,
    width = opts.width or 56,
    height = opts.height or "fit",
    layout = layout,
  })
  handle.close = close
  handle.submit = submit
  handle.body = body
  handle.actions = action_leaf
  action_leaf:focus()
  return handle
end

return M
