-- Context inspection is local: it never adds a message or makes a model request.
local DIM = { fg = "Comment" }
local function span(text, style) return { text = text, style = style } end
local function fmt(bytes) return smelt.text.format_tokens(math.ceil(bytes / 4)) end
local function muted(group, amount)
  local color, neutral = smelt.theme.rgb(group), smelt.theme.rgb("Comment")
  local rgb = {}
  for i = 1, 3 do rgb[i] = color[i] * (1 - amount) + neutral[i] * amount end
  return { fg = rgb }
end

smelt.cmd.register("context", function()
  local data = smelt.session.context_breakdown()
  local window = smelt.session.status().context.window
  if window and window <= 0 then window = nil end
  local total = data.system_bytes + data.tool_bytes + data.history_bytes
  local capacity = window and window * 4 or total
  local scale = math.max(total, capacity, 1)
  local system = { name = "System", bytes = data.system_bytes, style = muted("SmeltHeading", 0.65) }
  local groups, prompt_groups = { system }, {}
  local categories = {
    skills = { name = "Skills", style = muted("SmeltSuccess", 0.12) },
    instructions = { name = "Instructions", style = muted("SmeltReasonMedium", 0.12) },
    plugins = { name = "Plugins", style = muted("SmeltReasonHigh", 0.12) },
  }
  -- Preserve the assembled prompt's section order for both the grid and legend.
  for _, section in ipairs(data.sections) do
    local category = categories[section.kind]
    if category then
      local group = prompt_groups[section.kind]
      if not group then
        group = { name = category.name, bytes = 0, style = category.style }
        prompt_groups[section.kind] = group
        groups[#groups + 1] = group
      end
      group.bytes = group.bytes + section.bytes
      system.bytes = system.bytes - section.bytes
    end
  end
  groups[#groups + 1] = { name = "Tools", bytes = data.tool_bytes, style = muted("SmeltReasonUltra", 0.55) }
  groups[#groups + 1] = { name = "History", bytes = data.history_bytes, style = muted("SmeltLink", 0.12) }
  if window then
    groups[#groups + 1] = { name = "Free", bytes = math.max(0, capacity - total), style = DIM, free = true }
  end

  local width = math.max(20, math.min(60, smelt.ui.size().width - 4))
  local columns, rows = math.floor(width / 2), 8
  local cells = columns * rows
  local heading = "~" .. fmt(total)
  if window then
    heading = heading .. " / " .. smelt.text.format_tokens(window)
      .. string.format("  %.1f%%", total / capacity * 100)
  else
    heading = heading .. "  ·  window unknown"
  end
  if data.native then heading = heading .. "  ·  partial" end
  local lines = { { span(heading, { fg = total > capacity and "WarningMsg" or "Normal" }) }, {} }

  -- Round cumulative boundaries so the grid always contains exactly `cells` squares.
  local cumulative = 0
  for _, group in ipairs(groups) do
    cumulative = cumulative + group.bytes
    group.last = math.floor(cumulative / scale * cells + 0.5)
  end
  local index = 1
  for row = 1, rows do
    local line = {}
    for column = 1, columns do
      local cell = (row - 1) * columns + column
      while index < #groups and cell > groups[index].last do index = index + 1 end
      local group = groups[index]
      local empty = cell > group.last
      line[#line + 1] = span((group.free or empty) and "· " or "■ ", empty and DIM or group.style)
    end
    lines[#lines + 1] = line
  end
  lines[#lines + 1] = {}
  local legend_columns = width >= 44 and 2 or 1
  local legend_width = math.floor(width / legend_columns)
  local line = {}
  for _, group in ipairs(groups) do
    if group.bytes > 0 or group.free then
      local label = string.format("%-12s %s", group.name, fmt(group.bytes))
      line[#line + 1] = span(group.free and "· " or "■ ", group.style)
      line[#line + 1] = span(label .. string.rep(" ", math.max(1, legend_width - #label - 2)), DIM)
      if #line == legend_columns * 2 then lines[#lines + 1] = line; line = {} end
    end
  end
  if #line > 0 then lines[#lines + 1] = line end

  smelt.dialog.viewer({
    title = "context",
    styled = lines,
    max_height = "85%",
    keymaps = { { key = "<Esc>", on_press = function(ctx) ctx.close() end } },
  })
end, { desc = "show context usage as a colored grid" })
