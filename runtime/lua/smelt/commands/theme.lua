-- `/theme` - preview bundled UI + syntax themes.

local names, items = {}, {}
for i, scheme in ipairs(smelt.theme.list()) do
  names[i] = scheme.name
  local detail = scheme.detail or scheme.syntax or scheme.module
  items[i] = {
    label = scheme.name,
    description = detail,
    prefix = scheme.light and "☀ " or "☾ ",
  }
end

local original
local active_preview

local function snapshot()
  return {
    syntax = smelt.theme.syntax_theme(),
    light = smelt.theme.is_light(),
    groups = smelt.theme.snapshot(),
  }
end

local function restore()
  if original then smelt.theme.apply(original) end
  active_preview = nil
end

local function apply(name)
  local ok, err = pcall(smelt.theme.use, name)
  if not ok then
    smelt.notify.error(tostring(err))
    return false
  end
  active_preview = name
  return true
end

smelt.cmd.register_picker("theme", {
  desc = "preview UI and syntax themes",
  args = names,
  items = items,
  apply = function(arg)
    if not original then original = snapshot() end
    if apply(arg) then
      smelt.notify.info("theme preview is session-local; add smelt.theme.use(\"" .. arg .. "\") to init.lua to keep it")
    end
  end,
  prepare = function()
    original = snapshot()
    active_preview = nil
  end,
  on_select = function(item)
    if item and item.label and item.label ~= active_preview then apply(item.label) end
  end,
  on_enter = function(item)
    if item and item.label then
      apply(item.label)
      smelt.notify.info("theme preview selected for this session: " .. item.label)
    end
    original = nil
  end,
  on_dismiss = restore,
})
