-- Internal argv-style splitter for bundled slash commands. This is not a
-- shell parser: it does not expand variables, globs, command substitutions,
-- comments, redirects, or shell operators.

local M = {}

function M.split(input)
  local args = {}
  local buf = {}
  local quote = nil
  local escaped = false
  local token_started = false

  for ch in tostring(input or ""):gmatch(".") do
    if escaped then
      buf[#buf + 1] = ch
      escaped = false
      token_started = true
    elseif ch == "\\" and quote ~= "'" then
      escaped = true
      token_started = true
    elseif quote then
      if ch == quote then
        quote = nil
      else
        buf[#buf + 1] = ch
      end
      token_started = true
    elseif ch == "'" or ch == '"' then
      quote = ch
      token_started = true
    elseif ch:match("%s") then
      if token_started then
        args[#args + 1] = table.concat(buf)
        buf = {}
        token_started = false
      end
    else
      buf[#buf + 1] = ch
      token_started = true
    end
  end

  if escaped then return nil, "trailing escape" end
  if quote then return nil, "unterminated quote" end
  if token_started then args[#args + 1] = table.concat(buf) end
  return args, nil
end

return M
