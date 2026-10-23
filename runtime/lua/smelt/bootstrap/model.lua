if smelt and smelt.model then
  local function prefs() return smelt.state.persistent("model_preferred") end
  -- Read or update a named model preference. With only `name`, returns the
  -- stored model key or `nil`. Passing a second argument stores that model key;
  -- passing `nil` explicitly clears it. Preferences survive Lua reloads through
  -- `smelt.state.persistent("model_preferred")`.
  ---@type fun(name: string, ...: string?): string?
  smelt.model.preferred = function(name, ...)
    if select("#", ...) == 0 then
      return prefs()[name]
    end
    local value = ...
    prefs()[name] = value
    return value
  end
end
