-- Shared label/value wrapping for compact dialog headers.
-- Produces rows shaped as `label .. separator .. value`, with wrapped and
-- explicit continuation lines aligned to the start of the value column.

local M = {}

local function split_lines(text)
  local out = {}
  for line in (tostring(text or "") .. "\n"):gmatch("(.-)\n") do
    out[#out + 1] = line
  end
  return out
end

local function char_starts(text)
  local starts = {}
  local ok = pcall(function()
    for pos in utf8.codes(text) do starts[#starts + 1] = pos end
  end)
  if not ok then
    starts = {}
    for i = 1, #text do starts[#starts + 1] = i end
  end
  starts[#starts + 1] = #text + 1
  return starts
end

local function take_width(text, width)
  if width <= 0 or text == "" then return "", text end

  local starts = char_starts(text)
  local last = 0
  for i = 1, #starts - 1 do
    local end_byte = starts[i + 1] - 1
    if smelt.text.width(text:sub(1, end_byte)) > width then break end
    last = end_byte
  end
  if last == 0 then last = starts[2] and starts[2] - 1 or #text end
  return text:sub(1, last), text:sub(last + 1)
end

local function wrap_plain_line(line, width)
  if width <= 0 or smelt.text.width(line) <= width then return { line } end

  local out = {}
  local rest = line
  while rest ~= "" and smelt.text.width(rest) > width do
    local head, tail = take_width(rest, width)
    local cut = #head
    local best
    for s, e in head:gmatch("()%s+()") do
      if s > 1 and head:sub(1, s - 1):find("%S") then best = { s, e } end
    end
    if best then
      cut = best[1] - 1
      tail = rest:sub(best[2]):gsub("^%s+", "")
    end
    if cut <= 0 then
      head, tail = take_width(rest, width)
    else
      head = rest:sub(1, cut)
    end
    out[#out + 1] = head
    rest = tail
  end
  out[#out + 1] = rest
  return out
end

function M.initial_dialog_width(default_width)
  local ok, size = pcall(function() return smelt.ui.size() end)
  if ok and type(size) == "table" and tonumber(size.width) then
    return math.max(1, tonumber(size.width) - 2)
  end
  return default_width or 78
end

local function pad_right(text, width)
  text = tostring(text or "")
  local pad = width - smelt.text.width(text)
  if pad <= 0 then return text end
  return text .. string.rep(" ", pad)
end

function M.rows(label, value, width, opts)
  opts = opts or {}
  local separator = opts.separator or "  "
  local label_text = tostring(label or "")
  if opts.label_width then
    label_text = pad_right(label_text, tonumber(opts.label_width) or 0)
  end
  local prefix = label_text .. separator
  local indent = string.rep(" ", smelt.text.width(prefix))
  local value_width = math.max(1, (tonumber(width) or 80) - smelt.text.width(prefix))
  local rows = {}

  for line_idx, line in ipairs(split_lines(value)) do
    local chunks = wrap_plain_line(line, value_width)
    for chunk_idx, chunk in ipairs(chunks) do
      local first = line_idx == 1 and chunk_idx == 1
      rows[#rows + 1] = {
        label = first and prefix or indent,
        value = chunk,
        is_first = first,
      }
    end
  end
  return rows
end

function M.plain_lines(label, value, width, opts)
  local lines = {}
  for _, row in ipairs(M.rows(label, value, width, opts)) do
    lines[#lines + 1] = row.label .. row.value
  end
  return lines
end

function M.styled_lines(label, value, width, opts)
  opts = opts or {}
  local label_style = opts.label_style or { dim = true }
  local value_style = opts.value_style
  local syntax = opts.syntax
  local out = {}
  for _, row in ipairs(M.rows(label, value, width, opts)) do
    local spans = { { text = row.label, style = label_style } }
    local value_span = { text = row.value }
    if syntax then value_span.syntax = syntax end
    if value_style then value_span.style = value_style end
    spans[#spans + 1] = value_span
    out[#out + 1] = spans
  end
  return out
end

smelt.label_value = M

return M
