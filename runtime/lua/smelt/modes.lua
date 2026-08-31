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

---@class smelt.mode.Mode
---@field name string
---@field label string
---@field icon string
---@field hl_group string
---@field note string
---@field permissions table

local function copy_permissions(permissions)
  local out = {}
  for key, value in pairs(permissions or {}) do out[key] = value end
  return out
end

local function copy_mode(mode)
  if not mode then return nil end
  return {
    name = mode.name,
    label = mode.label,
    icon = mode.icon,
    hl_group = mode.hl_group,
    note = mode.note,
    permissions = copy_permissions(mode.permissions),
  }
end

local function normalize(spec)
  if type(spec) ~= "table" or type(spec.name) ~= "string" or spec.name == "" then
    error("smelt.mode.register requires { name = string }")
  end
  local mode = {
    name = spec.name,
    label = spec.label or spec.name,
    icon = spec.icon or "",
    hl_group = spec.hl_group or "SmeltModeDefault",
    note = spec.note or ("now in " .. spec.name .. " mode"),
    permissions = copy_permissions(spec.permissions),
  }
  return mode
end

-- Register an agent mode from `{ name, label?, icon?, hl_group?, note?,
-- permissions?, after? }`. Registering a new name appends it, or inserts it
-- after an existing `after` name. Registering an existing name replaces its
-- definition without changing its position. Raises when `name` is missing.
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

-- Return a copy of the registered mode definition for `name`, or `nil` when unknown.
---@type fun(name: string): smelt.mode.Mode?
smelt.mode.get = function(name)
  return copy_mode(registry[name])
end

-- Return copies of registered mode definitions in registration order.
---@type fun(): smelt.mode.Mode[]
smelt.mode.list = function()
  local out = {}
  for _, name in ipairs(order) do
    out[#out + 1] = copy_mode(registry[name])
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
---@tier ui_host
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
---@tier ui_host
---@type fun(): nil
smelt.reasoning.cycle = function()
  local list = smelt.reasoning.cycle_list()
  -- Empty list = no configured cycle; leave effort unchanged.
  if not list or #list == 0 then return end
  local nxt = next_in_cycle(list, smelt.reasoning.current())
  if nxt then smelt.reasoning.set(nxt) end
end
