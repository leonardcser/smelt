-- Notifications of its own: one quiet line at the top right for every tool smelt runs and when a
-- turn ends. It slides in, gets a tick when the tool is done, and slides away a few seconds later.
local LIFE, SLIDE = 3500, 200
local CARD, TEXT, DIM = { 34, 35, 44 }, { 225, 225, 232 }, { 130, 130, 148 }
local toasts, running, paint = {}, {}, nil

local function push(t)
  t.born = smelt.time.monotonic_ms()
  toasts[#toasts + 1] = t
  paint:invalidate()
  return t
end

smelt.events.on("tool_start", function(e)
  local args = e.args or {}
  local what = args.path or args.file_path or args.command or args.pattern or ""
  what = what:gsub("^" .. smelt.os.cwd():gsub("%p", "%%%0") .. "/", "")
  running[e.tool] = push({ title = e.tool, detail = what, mark = "·" })
end)
smelt.events.on("tool_end", function(e)
  local t = running[e.tool]
  if not t then return end
  running[e.tool] = nil
  t.mark, t.born = e.is_error and "✗" or "✓", smelt.time.monotonic_ms()
  paint:invalidate()
end)
smelt.events.on("turn_end", function()
  push({ title = "done", detail = "your turn", mark = "●" })
end)

-- How far a toast has slid in, from 0 to 1, easing at both ends of its life.
local function shown(age)
  local k = math.max(0, math.min(age / SLIDE, 1, (LIFE - age) / SLIDE))
  return 1 - (1 - k) ^ 3
end

local MARKS = { ["✓"] = { 120, 210, 140 }, ["✗"] = { 240, 100, 100 }, ["●"] = { 130, 150, 255 } }

paint = smelt.paint.frame(function(s)
  local now, live = smelt.time.monotonic_ms(), {}
  for _, t in ipairs(toasts) do
    if now - t.born < LIFE then live[#live + 1] = t end
  end
  toasts = live
  local y = 1
  for i = #toasts, math.max(1, #toasts - 4), -1 do
    local t = toasts[i]
    local text = " " .. t.mark .. " " .. t.title .. "  " .. smelt.text.truncate_cells(t.detail, 22) .. " "
    local w = smelt.text.width(text)
    local x = s:width() - 1 - math.floor((w + 1) * shown(now - t.born))
    local col = s:put_str(x, y, " " .. t.mark .. " ", { fg = MARKS[t.mark] or DIM, bg = CARD, bold = true })
    col = s:put_str(col, y, t.title, { fg = TEXT, bg = CARD })
    s:put_str(col, y, "  " .. smelt.text.truncate_cells(t.detail, 22) .. " ", { fg = DIM, bg = CARD })
    y = y + 2
  end
  if #toasts > 0 then paint:invalidate_after(16) end
end, { name = "toasts", order = 50 })
