-- `/color` - set the current session's task-slug color.

local presets = smelt.theme.presets
local preset_names, items = {}, {}
for i, p in ipairs(presets) do
  preset_names[i] = p.name
  items[i] = { label = p.name, description = p.detail, ansi_color = p.ansi, prefix = "● " }
end

-- Apply `ansi` to the slug pill bg and the bar/separator fg (prompt
-- top + bottom bars and the statusline dot separator all read from
-- `SmeltSeparator`), so `/color` tints the whole chrome family at once.
local function apply(ansi)
  local slug = smelt.theme.get("SmeltSlug") or {}
  smelt.theme.set("SmeltSlug", {
    fg = slug.fg,
    bg = ansi and { ansi = ansi } or nil,
    bold = slug.bold,
    italic = slug.italic,
    dim = slug.dim,
    underline = slug.underline,
    crossedout = slug.crossedout,
  })
  local separator = smelt.theme.get("SmeltSeparator") or {}
  smelt.theme.set("SmeltSeparator", {
    fg = ansi and { ansi = ansi } or nil,
    bg = separator.bg,
    bold = separator.bold,
    italic = separator.italic,
    dim = separator.dim,
    underline = separator.underline,
    crossedout = separator.crossedout,
  })
end

local original
smelt.cmd.picker("color", {
  desc       = "set session color",
  args       = preset_names,
  items      = items,
  apply      = function(arg)
    for _, p in ipairs(presets) do
      if p.name == arg then
        apply(p.ansi)
        return
      end
    end
    smelt.notify.error("unknown color: " .. arg)
  end,
  prepare    = function()
    original = {
      slug_bg = ((smelt.theme.get("SmeltSlug") or {}).bg or {}).ansi,
      separator_fg = ((smelt.theme.get("SmeltSeparator") or {}).fg or {}).ansi,
    }
  end,
  on_select  = function(item) if item.ansi_color then apply(item.ansi_color) end end,
  on_enter   = function(item) smelt.cmd.run("/color " .. item.label) end,
  on_dismiss = function()
    -- Restore both groups to their pre-picker values.
    local slug = smelt.theme.get("SmeltSlug") or {}
    smelt.theme.set("SmeltSlug", {
      fg = slug.fg,
      bg = original.slug_bg and { ansi = original.slug_bg } or nil,
      bold = slug.bold, italic = slug.italic, dim = slug.dim,
      underline = slug.underline, crossedout = slug.crossedout,
    })
    local separator = smelt.theme.get("SmeltSeparator") or {}
    smelt.theme.set("SmeltSeparator", {
      fg = original.separator_fg and { ansi = original.separator_fg } or nil,
      bg = separator.bg,
      bold = separator.bold, italic = separator.italic, dim = separator.dim,
      underline = separator.underline, crossedout = separator.crossedout,
    })
  end,
})
