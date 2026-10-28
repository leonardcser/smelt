-- `/model` - switch active model. Direct with an arg; picker without.

local function models_list()
  return smelt.model.list() or {}
end

local function build_items()
  local out = {}
  for _, m in ipairs(models_list()) do
    out[#out + 1] = {
      label        = m.display_name or m.name,
      description  = m.provider,
      search_terms = (m.key or "") .. " " .. (m.name or "") .. " " .. (m.provider or ""),
      _key         = m.key,
    }
  end
  return out
end

-- Read on each completion: providers can list their models after startup.
local function model_keys()
  local keys = {}
  for _, m in ipairs(models_list()) do
    keys[#keys + 1] = m.key
  end
  return keys
end

smelt.cmd.register_picker("model", {
  desc     = "switch model",
  args_fn  = model_keys,
  items    = build_items,
  apply    = function(arg) smelt.model.set(arg) end,
  prepare  = function()
    if #models_list() == 0 then smelt.notify.error("no models available") end
  end,
  on_enter = function(item) if item._key then smelt.cmd.run("/model " .. item._key) end end,
})
