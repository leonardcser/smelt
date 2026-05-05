-- Lua-side cycle logic for agent mode and reasoning effort. Replaces
-- the seed `smelt.mode.cycle` / `smelt.reasoning.cycle` no-op stubs
-- registered by the Rust bindings. Reads the configured cycle list
-- from Rust, finds the current value, and calls `set` with the next
-- entry. Also owns the mode-icon registry the statusline reads.

local function next_in_cycle(list, current)
  for i, v in ipairs(list) do
    if v == current then
      return list[(i % #list) + 1]
    end
  end
  return list[1]
end

local mode_icons = {
  normal = "○ ",
  plan = "◇ ",
  apply = "→ ",
  yolo = "⚡",
}

smelt.mode.icon = function(name)
  return mode_icons[name] or ""
end

smelt.mode.set_icon = function(name, icon)
  mode_icons[name] = icon
end

smelt.mode.cycle = function()
  local list = smelt.mode.cycle_list()
  if not list or #list == 0 then return end
  local nxt = next_in_cycle(list, smelt.mode.get())
  if nxt then smelt.mode.set(nxt) end
end

smelt.reasoning.cycle = function()
  local list = smelt.reasoning.cycle_list()
  -- Empty cycle = leave reasoning unchanged. Mirrors the historical
  -- `cycle_within` behaviour: with no allowed list, the effort
  -- doesn't move.
  if not list or #list == 0 then return end
  local nxt = next_in_cycle(list, smelt.reasoning.get())
  if nxt then smelt.reasoning.set(nxt) end
end
