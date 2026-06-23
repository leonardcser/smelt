-- Cycle logic for agent mode and reasoning effort; owns the mode registry.

local function next_in_cycle(list, current)
  for i, v in ipairs(list) do
    if v == current then
      return list[(i % #list) + 1]
    end
  end
  return list[1]
end

local registry = {}
local order = {}

local function note_for(name)
  local mode = registry[name]
  return mode and mode.note or ("now in " .. name .. " mode")
end

local function normalize(spec)
  if type(spec) ~= "table" or type(spec.name) ~= "string" or spec.name == "" then
    error("smelt.mode.register requires { name = string }")
  end
  local mode = {
    name = spec.name,
    label = spec.label or spec.name,
    icon = spec.icon or "",
    hl_group = spec.hl_group or ("SmeltMode" .. spec.name:gsub("^%l", string.upper)),
    note = spec.note or ("now in " .. spec.name .. " mode"),
    permissions = spec.permissions or {},
  }
  return mode
end

---@type fun(spec: table): nil
smelt.mode.register = function(spec)
  local mode = normalize(spec)
  if not registry[mode.name] then
    local inserted = false
    if spec.after then
      for i, name in ipairs(order) do
        if name == spec.after then
          table.insert(order, i + 1, mode.name)
          inserted = true
          break
        end
      end
    end
    if not inserted then order[#order + 1] = mode.name end
  end
  registry[mode.name] = mode
end

---@type fun(name: string): table|nil
smelt.mode.get = function(name)
  return registry[name]
end

---@type fun(): table[]
smelt.mode.list = function()
  local out = {}
  for _, name in ipairs(order) do
    out[#out + 1] = registry[name]
  end
  return out
end

---@type fun(name: string): string
smelt.mode.icon = function(name)
  local mode = registry[name]
  return mode and mode.icon or ""
end

---@type fun(name: string, icon: string): nil
smelt.mode.set_icon = function(name, icon)
  local mode = registry[name]
  if not mode then
    smelt.mode.register({ name = name, icon = icon })
  else
    mode.icon = icon
  end
end

---@type fun(name: string): table
smelt.mode.style = function(name)
  local mode = registry[name]
  return { hl_group = mode and mode.hl_group or "SmeltModeDefault" }
end

---@type fun(name: string): string
smelt.mode.note = function(name)
  return note_for(name)
end

---@type fun(): table<string, table>
smelt.mode.permission_behaviors = function()
  local out = {}
  for name, mode in pairs(registry) do
    out[name] = mode.permissions or {}
  end
  return out
end

smelt.mode.register({
  name = "normal",
  icon = "○ ",
  hl_group = "SmeltModeDefault",
  note = "now in normal mode.",
  permissions = {
    default_decision = "ask",
    allow_subcommands_by_default = false,
    ask_on_output_redirection = true,
  },
})

smelt.mode.register({
  name = "apply",
  icon = "→ ",
  hl_group = "SmeltModeApply",
  note = "now in apply mode. You may read, edit, and create files. Continue to confirm destructive bash commands before running them.",
  permissions = {
    default_decision = "ask",
    allow_subcommands_by_default = false,
    ask_on_output_redirection = false,
  },
})

smelt.mode.register({
  name = "yolo",
  icon = "⚡",
  hl_group = "SmeltModeYolo",
  note = "now in yolo mode. Full autonomy; act without pausing for confirmation. Continue to avoid genuinely irreversible operations.",
  permissions = {
    default_decision = "allow",
    allow_subcommands_by_default = true,
    ask_on_output_redirection = false,
  },
})

-- Advance the active agent mode to the next entry in `smelt.mode.cycle_list()`,
-- wrapping at the end. No-op when the cycle is empty.
---@type fun(): nil
smelt.mode.cycle = function()
  local list = smelt.mode.cycle_list()
  if not list or #list == 0 then return end
  local nxt = next_in_cycle(list, smelt.mode.current())
  if nxt then smelt.mode.set(nxt) end
end

-- Advance the active reasoning effort to the next entry in
-- `smelt.reasoning.cycle_list()`, wrapping at the end. No-op when the
-- cycle is empty.
---@type fun(): nil
smelt.reasoning.cycle = function()
  local list = smelt.reasoning.cycle_list()
  -- Empty list = no configured cycle; leave effort unchanged.
  if not list or #list == 0 then return end
  local nxt = next_in_cycle(list, smelt.reasoning.current())
  if nxt then smelt.reasoning.set(nxt) end
end
