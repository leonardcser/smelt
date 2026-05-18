-- `/color` — change the task-slug label color (same shape as /theme).

local presets = smelt.theme.presets
local preset_names, items = {}, {}
for i, p in ipairs(presets) do
  preset_names[i] = p.name
  items[i] = { label = p.name, description = p.detail, ansi_color = p.ansi, prefix = "● " }
end

local function set_slug(ansi)
  local current = smelt.theme.get("SmeltSlug") or {}
  smelt.theme.set("SmeltSlug", {
    fg = current.fg,
    bg = ansi and { ansi = ansi } or nil,
    bold = current.bold,
    italic = current.italic,
    dim = current.dim,
    underline = current.underline,
    crossedout = current.crossedout,
  })
end

local original_ansi
smelt.cmd.picker("color", {
  desc       = "set task slug color",
  args       = preset_names,
  items      = items,
  apply      = function(arg)
    for _, p in ipairs(presets) do
      if p.name == arg then
        set_slug(p.ansi)
        return
      end
    end
    smelt.notify.error("unknown color: " .. arg)
  end,
  prepare    = function() original_ansi = ((smelt.theme.get("SmeltSlug") or {}).bg or {}).ansi end,
  on_select  = function(item) if item.ansi_color then set_slug(item.ansi_color) end end,
  on_enter   = function(item) smelt.cmd.run("/color " .. item.label) end,
  on_dismiss = function() set_slug(original_ansi) end,
})
