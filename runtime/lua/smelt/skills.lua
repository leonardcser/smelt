-- Helpers for skill-backed Lua commands.

local M = smelt.skills or {}

function M.body(name)
  local content, err = M.content(name)
  if not content then return nil, err end
  content = content:gsub('^<skill name="[^"]+">\n?', '')
  content = content:gsub('\n?</skill>%s*$', '')
  return content
end

return M
