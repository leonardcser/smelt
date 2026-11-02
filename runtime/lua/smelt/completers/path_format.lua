local M = {}

function M.item_path(item)
  return item.insert_text or item.path or item.label or ""
end

function M.quote_at(text)
  if text:find(" ", 1, true) then
    return '@"' .. text .. '"'
  end
  return "@" .. text
end

function M.quote_path_token(text)
  if text:find("%s") then
    return '"' .. text:gsub('"', '\\"') .. '"'
  end
  return text
end

return M
