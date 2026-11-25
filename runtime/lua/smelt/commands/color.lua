-- `/color` - set the current session's task-slug color.

local colors, items = {}, { { label = "default", description = "clear session color", prefix = "○ " } }
for _, p in ipairs(smelt.theme.presets) do
  colors[p.name] = { slug_bg = p.ansi, separator_fg = p.ansi }
  items[#items + 1] = { label = p.name, description = p.detail, prefix_style = { fg = p.ansi }, prefix = "● " }
end

local function read_colors()
  return {
    slug_bg = smelt.theme.get("SmeltSlug").bg,
    separator_fg = smelt.theme.get("SmeltSeparator").fg,
  }
end

-- Tint the slug pill, prompt bars, and statusline separator without changing
-- other style attributes.
local function write_colors(color)
  local slug = smelt.theme.get("SmeltSlug")
  slug.bg = color.slug_bg
  smelt.theme.set("SmeltSlug", slug)
  local separator = smelt.theme.get("SmeltSeparator")
  separator.fg = color.separator_fg
  smelt.theme.set("SmeltSeparator", separator)
end

-- A Lua reload can keep the active tint, so keep its baseline across reloads too.
local state = smelt.state.get("smelt.commands.color")
local before_picker, picker_default

local function default_colors(current)
  local result = {}
  for _, key in ipairs({ "slug_bg", "separator_fg" }) do
    -- A theme switch or another override can replace a field we set.
    if state.applied and current[key] == state.applied[key] then
      result[key] = state.underlying[key]
    else
      result[key] = current[key]
    end
  end
  return result
end

local function select_color(name, defaults)
  if name == "default" then
    write_colors(defaults)
    state.applied, state.underlying = nil, nil
  elseif colors[name] then
    write_colors(colors[name])
    state.applied, state.underlying = colors[name], defaults
  else
    smelt.notify.error("unknown color: " .. name)
  end
end

smelt.cmd.register_picker("color", {
  desc       = "set or clear session color",
  items      = items,
  apply      = function(arg) select_color(arg, default_colors(read_colors())) end,
  prepare    = function()
    before_picker = read_colors()
    picker_default = default_colors(before_picker)
  end,
  on_select  = function(item) write_colors(colors[item.label] or picker_default) end,
  on_enter   = function(item) select_color(item.label, picker_default) end,
  on_dismiss = function() write_colors(before_picker) end,
})
