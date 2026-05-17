-- Cycle logic for agent mode and reasoning effort; owns the mode-icon registry.

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
  local nxt = next_in_cycle(list, smelt.mode())
  if nxt then smelt.mode(nxt) end
end

smelt.reasoning.cycle = function()
  local list = smelt.reasoning.cycle_list()
  -- Empty list = no configured cycle; leave effort unchanged.
  if not list or #list == 0 then return end
  local nxt = next_in_cycle(list, smelt.reasoning())
  if nxt then smelt.reasoning(nxt) end
end
