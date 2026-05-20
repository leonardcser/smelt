-- `/settings` — every key in `smelt.settings.schema()` shows up here:
--   * bool                   → Enter toggles
--   * string with `choices`  → Enter cycles to the next choice
--   * number                 → shown read-only; tune via init.lua, `--set`,
--                              or `smelt.settings.<key> = N`
--
-- The list is schema-driven on purpose. Every newly declared setting in
-- `smelt_core::config::SETTINGS` shows up here automatically — no hand
-- list to keep in sync.

local function humanise(key)
  return (key:gsub("_", " "))
end

local function describe(row)
  local v = smelt.settings[row.key]
  if row.kind == "bool" then
    return v and "on" or "off"
  end
  return tostring(v)
end

local function next_choice(choices, current)
  for i, c in ipairs(choices) do
    if c == current then return choices[(i % #choices) + 1] end
  end
  return choices[1]
end

local function build_items()
  local items = {}
  for _, row in ipairs(smelt.settings.schema()) do
    items[#items + 1] = {
      label        = humanise(row.key),
      description  = describe(row),
      search_terms = row.key,
      _row         = row,
    }
  end
  return items
end

smelt.cmd.picker("settings", {
  desc       = "open settings menu",
  items      = build_items,
  on_enter   = function(item)
    local row = item._row
    if not row then return end
    if row.kind == "bool" then
      smelt.settings[row.key] = not smelt.settings[row.key]
    elseif row.kind == "string" and row.choices then
      smelt.settings[row.key] = next_choice(row.choices, smelt.settings[row.key])
    end
    -- numbers are read-only in the picker; no-op
  end,
  stay_open  = true,
  startup_ok = true,
})
