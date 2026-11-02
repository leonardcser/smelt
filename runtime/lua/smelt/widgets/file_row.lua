local M = {}

-- Keep display fields separate from provider data used for ranking and acceptance.
function M.picker(item)
  local prefix, style = item.prefix, item.prefix_style
  if prefix == nil and item.path and smelt.settings.file_icons then
    local icon, icon_style
    if item.kind == "dir" then
      icon = ""
      -- Directory is optional in themes; get supplies the neutral default.
      if smelt.settings.file_icon_colors then icon_style = smelt.theme.get("Directory") end
    else
      local color
      icon, color = smelt.path.icon(item.path, {
        colors = smelt.settings.file_icon_colors,
        light = smelt.theme.is_light(),
      })
      if color then icon_style = { fg = color } end
    end
    if icon then prefix = icon .. " " end
    if style == nil then style = icon_style end
  end
  return {
    label = item.label:gsub("%c", " "),
    description = item.description,
    prefix = prefix,
    prefix_style = style,
    label_style = item.label_style,
  }
end

function M.render(item)
  local row = M.picker(item)
  local spans = {
    { text = row.prefix or "", style = row.prefix_style },
    { text = row.label, style = row.label_style },
  }
  if row.description then
    spans[#spans + 1] = { text = "  " .. row.description, style = { dim = true } }
  end
  return { spans = spans }
end

return M
