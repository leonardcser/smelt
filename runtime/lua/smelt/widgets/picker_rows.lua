return function(items, opts)
  local rows = {}
  for i, item in ipairs(items) do
    local display = item
    if opts.render and not (type(item) == "table" and item._synthetic) then
      display = opts.render(item)
    end
    if type(display) == "string" then display = { label = display } end
    assert(type(display) == "table", "render must return a picker row or string")
    local row = {
      label = display.label,
      description = display.description,
      prefix = display.prefix,
      prefix_style = display.prefix_style,
      label_style = display.label_style,
    }
    if row.prefix_style == nil then row.prefix_style = opts.prefix_style end
    if row.label_style == nil then row.label_style = opts.label_style end
    if row.prefix == nil then row.prefix = opts.prefix end
    rows[i] = row
  end
  return rows
end
