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

local model_keys = {}
for _, m in ipairs(models_list()) do
  model_keys[#model_keys + 1] = m.key
end

smelt.cmd.picker("model", {
  desc     = "switch model",
  args     = model_keys,
  items    = build_items,
  apply    = function(arg) smelt.model.set(arg) end,
  prepare  = function()
    if #models_list() == 0 then smelt.notify.error("no models available") end
  end,
  on_enter = function(item) if item._key then smelt.cmd.run("/model " .. item._key) end end,
})
