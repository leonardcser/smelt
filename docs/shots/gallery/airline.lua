-- A vim-airline statusline: powerline segments whose mode block changes color with what smelt is
-- doing (blue at rest, orange at work, green when done), the project and branch beside it, and the
-- model, context and time on the right.
local STATE = {
  working = { "WORKING", { 250, 150, 70 } },
  busy = { "WORKING", { 250, 150, 70 } },
  done = { "DONE", { 140, 210, 110 } },
}
local REST = { "READY", { 110, 160, 250 } }
local MID, LOW, FG, DARK = { 70, 72, 90 }, { 40, 41, 52 }, { 220, 222, 235 }, { 22, 22, 30 }
local branch = ""
local function refresh()
  smelt.spawn(function()
    local snap = smelt.git.diff()
    branch = snap and snap.branch or ""
    smelt.statusline:invalidate()
  end)
end
refresh()

-- Segments left to right, each a color and its text; an arrow leads from one into the next.
local function segments(list, right)
  local runs = {}
  for i, seg in ipairs(list) do
    local nxt = list[i + 1]
    if right then
      runs[#runs + 1] = { text = "\u{e0b2}", style = { fg = seg[1], bg = (list[i - 1] or { nil })[1] } }
    end
    runs[#runs + 1] = { text = " " .. seg[2] .. " ", style = { fg = seg[3] or FG, bg = seg[1], bold = seg[4] } }
    if not right then
      runs[#runs + 1] = { text = "\u{e0b0}", style = { fg = seg[1], bg = nxt and nxt[1] } }
    end
  end
  return { runs = runs, align_right = right }
end

smelt.statusline:clear()
smelt.statusline:configure({ bg = "Normal" })
smelt.statusline:add("airline", function()
  local mode = STATE[smelt.signal.get("work_state")] or REST
  local used, window = smelt.session.context_tokens() or 0, smelt.session.context_window() or 1
  local model = (smelt.signal.get("model") or ""):gsub("^.*/", "")
  return {
    segments({
      { mode[2], mode[1], DARK, true },
      { MID, " " .. (branch ~= "" and branch or "main") },
      { LOW, smelt.path.basename(smelt.os.cwd()) },
    }),
    segments({
      { LOW, model },
      { MID, string.format("%d%%", math.floor(used / window * 100)) },
      { mode[2], os.date("%H:%M"), DARK, true },
    }, true),
  }
end)
for _, name in ipairs({ "work_state", "model", "tokens_used" }) do
  smelt.signal.subscribe(name, function() smelt.statusline:invalidate() end)
end
smelt.events.on("tool_end", refresh)
