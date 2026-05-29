-- Built-in /stats command.

local DIM = { fg = "Comment" }
local HEAD = { fg = "SmeltAccent", bold = true }
local VALUE = { fg = "Normal" }
local ACCENT = { fg = "SmeltAccent" }
local HEAT_EMPTY = { fg = "Comment", dim = true }

local function span(text, style) return { text = text, style = style } end
local function row(...) return { ... } end
local function append(dst, src) for _, line in ipairs(src) do dst[#dst + 1] = line end end

local function clamp(n)
  n = tonumber(n) or 0
  if n < 0 then return 0 end
  if n > 255 then return 255 end
  return math.floor(n + 0.5)
end

local function ansi_to_rgb(n)
  n = tonumber(n)
  if not n then return nil end
  local basic = {
    [0] = { 0, 0, 0 },       { 128, 0, 0 },     { 0, 128, 0 },     { 128, 128, 0 },
    { 0, 0, 128 },           { 128, 0, 128 },   { 0, 128, 128 },   { 192, 192, 192 },
    { 128, 128, 128 },       { 255, 0, 0 },     { 0, 255, 0 },     { 255, 255, 0 },
    { 0, 0, 255 },           { 255, 0, 255 },   { 0, 255, 255 },   { 255, 255, 255 },
  }
  if n >= 0 and n <= 15 then return basic[n] end
  if n >= 16 and n <= 231 then
    local v = n - 16
    local levels = { 0, 95, 135, 175, 215, 255 }
    local r = math.floor(v / 36) % 6
    local g = math.floor(v / 6) % 6
    local b = v % 6
    return { levels[r + 1], levels[g + 1], levels[b + 1] }
  end
  if n >= 232 and n <= 255 then
    local grey = 8 + (n - 232) * 10
    return { grey, grey, grey }
  end
  return nil
end

local function color_rgb(color)
  if type(color) ~= "table" then return nil end
  if type(color.rgb) == "table" then return { color.rgb[1], color.rgb[2], color.rgb[3] } end
  if color.ansi then return ansi_to_rgb(color.ansi) end
  if color[1] and color[2] and color[3] then return { color[1], color[2], color[3] } end
  return nil
end

local function mix(a, b, t)
  return {
    clamp(a[1] + (b[1] - a[1]) * t),
    clamp(a[2] + (b[2] - a[2]) * t),
    clamp(a[3] + (b[3] - a[3]) * t),
  }
end

local function heat_styles()
  local accent = color_rgb((smelt.theme.get("SmeltAccent") or {}).fg) or { 120, 160, 255 }
  local white = { 255, 255, 255 }
  return {
    { fg = { rgb = mix(accent, white, 0.55) } },
    { fg = { rgb = mix(accent, white, 0.25) } },
    { fg = { rgb = accent } },
  }
end

local function fmt(n)
  n = tonumber(n) or 0
  if n >= 1000000000 then return string.format("%.1fB", n / 1000000000) end
  if n >= 1000000 then return string.format("%.1fM", n / 1000000) end
  if n >= 1000 then return string.format("%.1fk", n / 1000) end
  return tostring(math.floor(n))
end

local function cost(usd)
  usd = tonumber(usd) or 0
  if usd < 0.01 then return string.format("$%.4f", usd) end
  if usd < 1 then return string.format("$%.3f", usd) end
  return string.format("$%.2f", usd)
end

local function day_key(ms) return math.floor((tonumber(ms) or 0) / 86400000) end
local function hour_key(ms) return math.floor((tonumber(ms) or 0) / 3600000) end

local function aggregate(entries)
  local stats = {
    calls = 0,
    prompt = 0,
    completion = 0,
    cost = 0,
    by_model = {},
    by_day = {},
    by_hour = {},
  }
  local now_ms = os.time() * 1000
  local h24_ago = now_ms - 86400000

  for _, e in ipairs(entries or {}) do
    local prompt = tonumber(e.prompt_tokens) or 0
    local completion = tonumber(e.completion_tokens) or 0
    local total = prompt + completion
    local c = tonumber(e.cost_usd) or 0
    local model = e.model or "unknown"
    stats.calls = stats.calls + 1
    stats.prompt = stats.prompt + prompt
    stats.completion = stats.completion + completion
    stats.cost = stats.cost + c

    local m = stats.by_model[model] or { model = model, calls = 0, prompt = 0, completion = 0, cost = 0 }
    m.calls = m.calls + 1
    m.prompt = m.prompt + prompt
    m.completion = m.completion + completion
    m.cost = m.cost + c
    stats.by_model[model] = m

    local day = day_key(e.timestamp_ms)
    stats.by_day[day] = (stats.by_day[day] or 0) + total
    if (tonumber(e.timestamp_ms) or 0) >= h24_ago then
      local hour = hour_key(e.timestamp_ms)
      stats.by_hour[hour] = (stats.by_hour[hour] or 0) + total
    end
  end
  return stats
end

local function kv_lines(items)
  local width = 0
  for _, item in ipairs(items) do width = math.max(width, #item[1]) end
  local lines = {}
  for _, item in ipairs(items) do
    lines[#lines + 1] = row(span(string.format("%-" .. width .. "s  ", item[1]), DIM), span(item[2], VALUE))
  end
  return lines
end

local SPARK = { " ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█" }
local function sparkline(values)
  local max = 1
  for _, v in ipairs(values) do max = math.max(max, v) end
  local out = {}
  for _, v in ipairs(values) do
    local idx = math.floor((v / max) * (#SPARK - 1) + 0.5) + 1
    out[#out + 1] = SPARK[math.max(1, math.min(#SPARK, idx))]
  end
  return table.concat(out)
end

local function summary_lines(stats)
  local total = stats.prompt + stats.completion
  local items = {}
  if stats.cost > 0 then items[#items + 1] = { "total cost", cost(stats.cost) } end
  items[#items + 1] = { "calls", tostring(stats.calls) }
  items[#items + 1] = { "tokens", string.format("%s (%s prompt + %s completion)", fmt(total), fmt(stats.prompt), fmt(stats.completion)) }
  if stats.calls > 0 then items[#items + 1] = { "avg/call", fmt(total / stats.calls) .. " tokens" } end
  return kv_lines(items)
end

local function model_lines(stats)
  local models = {}
  for _, m in pairs(stats.by_model) do
    m.total = m.prompt + m.completion
    models[#models + 1] = m
  end
  if #models <= 1 then return {} end
  table.sort(models, function(a, b) return a.total > b.total end)

  local show_cost = false
  for _, m in ipairs(models) do if m.cost > 0 then show_cost = true end end

  local model_width, calls_width, tokens_width, cost_width = 0, 0, 0, 0
  for _, m in ipairs(models) do
    model_width = math.max(model_width, #m.model)
    calls_width = math.max(calls_width, #tostring(m.calls))
    tokens_width = math.max(tokens_width, #fmt(m.total))
    cost_width = math.max(cost_width, #cost(m.cost))
  end

  local lines = { row(span("", DIM)), row(span("per model", HEAD)) }
  for _, m in ipairs(models) do
    local value = string.format(
      "%" .. calls_width .. "d calls  %" .. tokens_width .. "s tokens",
      m.calls,
      fmt(m.total)
    )
    if show_cost then value = value .. "  " .. string.format("%" .. cost_width .. "s", cost(m.cost)) end
    lines[#lines + 1] = row(span("  " .. string.format("%-" .. model_width .. "s", m.model) .. "  ", DIM), span(value, VALUE))
  end
  return lines
end

local function hourly_lines(stats)
  local now_hour = hour_key(os.time() * 1000)
  local values = {}
  for i = 0, 23 do
    local h = now_hour - 23 + i
    values[#values + 1] = stats.by_hour[h] or 0
  end
  return {
    row(span("last 24 hours", HEAD)),
    row(span(sparkline(values), ACCENT)),
    row(span("24h ago ─────────────── now", DIM)),
  }
end

local function daily_lines(stats)
  local today = day_key(os.time() * 1000)
  local values = {}
  local max = 1
  for i = 0, 83 do
    local v = stats.by_day[today - 83 + i] or 0
    values[i + 1] = v
    max = math.max(max, v)
  end

  local styles = heat_styles()
  local labels = { "Mo", "Tu", "We", "Th", "Fr", "Sa", "Su" }
  local lines = { row(span("", DIM)), row(span("daily activity (12 weeks)", HEAD)) }
  for row_idx, label in ipairs(labels) do
    local out = row(span(label .. "  ", DIM))
    for week = 0, 11 do
      local v = values[week * 7 + row_idx] or 0
      if v == 0 then
        out[#out + 1] = span("■", HEAT_EMPTY)
      else
        local level = math.max(1, math.min(#styles, math.floor((v / max) * (#styles - 1) + 1)))
        out[#out + 1] = span("■", styles[level])
      end
      if week < 11 then out[#out + 1] = span(" ", DIM) end
    end
    lines[#lines + 1] = out
  end
  return lines
end

local function text_width(text)
  return utf8.len(text or "") or #(text or "")
end

local function line_width(line)
  local width = 0
  for _, part in ipairs(line or {}) do width = width + text_width(part.text or "") end
  return width
end

local function merge_rows(left, right, gap)
  gap = gap or 6
  local left_width = 0
  for _, line in ipairs(left) do left_width = math.max(left_width, line_width(line)) end
  local rows = math.max(#left, #right)
  local lines = {}
  for i = 1, rows do
    local out = {}
    local l = left[i] or row(span("", DIM))
    for _, part in ipairs(l) do out[#out + 1] = part end
    out[#out + 1] = span(string.rep(" ", left_width - line_width(l) + gap), DIM)
    local r = right[i] or row(span("", DIM))
    for _, part in ipairs(r) do out[#out + 1] = part end
    lines[#lines + 1] = out
  end
  return lines
end

local function stats_lines()
  local entries = smelt.metrics.entries()
  if not entries or #entries == 0 then return { row(span("No metrics recorded yet.", HEAD)) } end
  local stats = aggregate(entries)
  local left = {}
  append(left, summary_lines(stats))
  append(left, model_lines(stats))
  local right = {}
  append(right, hourly_lines(stats))
  append(right, daily_lines(stats))
  return merge_rows(left, right)
end

smelt.cmd.register("stats", function()
  smelt.dialog.viewer({ title = "stats", styled = stats_lines(), wrap = false })
end, { desc = "show token usage statistics" })
