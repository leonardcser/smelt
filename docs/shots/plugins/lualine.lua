-- ~/.config/smelt/plugins/lualine.lua
local A, B, C = { 232, 100, 26 }, { 58, 47, 39 }, { 30, 26, 21 }
local FG, DARK = { 232, 223, 214 }, { 23, 20, 15 }
local DIFF = {
  { "added", " ", { 143, 191, 106 } },
  { "modified", " ", { 230, 180, 80 } },
  { "removed", " ", { 239, 107, 91 } },
}
local branch, counts = "", {}

-- Read the branch and count changed files after every tool the agent runs.
local function refresh()
  smelt.spawn(function()
    local snap = smelt.git.diff()
    branch, counts = snap and snap.branch or "", { added = 0, modified = 0, removed = 0 }
    for _, f in ipairs(snap and snap.diff:files() or {}) do
      local kind = (f.status == "?" or f.status == "A") and "added"
        or f.status == "D" and "removed" or "modified"
      counts[kind] = counts[kind] + 1
    end
    smelt.statusline.invalidate()
  end)
end
smelt.events.on("tool_end", refresh)
refresh()

local function text(items, s, bg, fg, bold)
  table.insert(items, { text = s, style = { bg = bg, fg = fg, bold = bold } })
end

smelt.statusline.remove("core")
smelt.statusline.add("line", function()
  local items = {}
  local cwd = smelt.path.basename(smelt.os.cwd())
  text(items, " " .. (smelt.signal.get("agent_mode") or "") .. " ", A, DARK, true)
  text(items, "", B, A)
  text(items, "  " .. cwd .. " ", B, FG)
  text(items, "", C, B)
  text(items, "  " .. branch .. " ", C, FG)
  for _, d in ipairs(DIFF) do
    local n = counts[d[1]] or 0
    if n > 0 then text(items, d[2] .. n .. " ", C, d[3]) end
  end
  return items
end)
