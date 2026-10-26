-- Shared spinner - pure Lua, loaded once so plugins see the same table.
if smelt.time and smelt.time.now_ms then
  require("smelt.spinner")
end

-- `smelt.theme` is UiHost. Only attach the convenience loader when it exists.
if smelt.theme then
  local bundled = require("smelt.colorschemes._two_face")
  local registry = {
    {
      name = "default",
      module = "default",
      syntax = "Monokai Extended",
      detail = "Smelt default",
    },
  }
  local syntax_names = {}
  for _, name in ipairs(smelt.theme.syntax_theme_names()) do syntax_names[name] = true end
  local bundled_syntax = {}
  for _, scheme in ipairs(bundled.schemes or {}) do
    if not syntax_names[scheme.syntax] then
      error("bundled colorscheme `" .. scheme.name .. "` references unknown syntax theme `" .. tostring(scheme.syntax) .. "`")
    end
    bundled_syntax[scheme.syntax] = true
    registry[#registry + 1] = scheme
  end
  for name in pairs(syntax_names) do
    if not bundled_syntax[name] then
      error("missing bundled colorscheme for syntax theme `" .. name .. "`")
    end
  end

  local function copy_scheme(scheme)
    if not scheme then return nil end
    return {
      name = scheme.name,
      module = scheme.module,
      syntax = scheme.syntax,
      light = scheme.light,
      detail = scheme.detail,
    }
  end

  local function resolve_scheme(name)
    for _, scheme in ipairs(registry) do
      if scheme.name == name or scheme.module == name then return scheme end
    end
    return nil
  end

  --- Return built-in colorschemes as `{ name, module, syntax, light, detail? }` rows.
  ---@type fun(): table[]
  function smelt.theme.list()
    local out = {}
    for i, scheme in ipairs(registry) do out[i] = copy_scheme(scheme) end
    return out
  end

  --- Return metadata for a built-in colorscheme by display name or module slug.
  ---@type fun(name: string): table?
  function smelt.theme.info(name)
    return copy_scheme(resolve_scheme(name))
  end

  -- Load colorscheme `name` from `runtime/lua/smelt/colorschemes/<name>.lua`
  -- and apply it. `name` may be either a module slug (`catppuccin-mocha`) or
  -- the display syntax theme name (`Catppuccin Mocha`).
  ---@tier ui_host
  ---@type fun(name: string): nil
  function smelt.theme.use(name)
    local scheme = resolve_scheme(name)
    local module = scheme and scheme.module or name
    local spec = require("smelt.colorschemes." .. module)
    if type(spec) ~= "table" then
      error("smelt.theme.use: colorscheme `" .. name .. "` must return a ThemeSpec table", 2)
    end
    smelt.theme.apply(spec)
  end

  -- Built-in session-color presets. Used by `/color`; user
  -- colorschemes can extend this list. Each entry is
  -- `{ name = string, detail = string, ansi = integer }`.
  smelt.theme.presets = smelt.theme.presets or {
    { name = "ember",    detail = "default",         ansi = 208 },
    { name = "coral",    detail = "salmon pink",     ansi = 210 },
    { name = "rose",     detail = "soft pink",       ansi = 211 },
    { name = "gold",     detail = "warm yellow",     ansi = 220 },
    { name = "ice",      detail = "cool white-blue", ansi = 159 },
    { name = "sky",      detail = "light blue",      ansi = 117 },
    { name = "blue",     detail = "classic blue",    ansi = 69  },
    { name = "lavender", detail = "cool purple",     ansi = 147 },
    { name = "lilac",    detail = "warm purple",     ansi = 183 },
    { name = "mint",     detail = "soft green",      ansi = 115 },
    { name = "sage",     detail = "muted green",     ansi = 108 },
    { name = "silver",   detail = "grey",            ansi = 244 },
  }
end
