-- `/cmd <arg>` completer — opens when the buffer reads `/<cmd> ` (trailing
-- space) and the cursor sits past that prefix, for any command that declared
-- positional argument labels via `smelt.cmd.register{ args = {...} }`. Tab
-- and Enter replace the in-progress arg with the selected label (no
-- automatic submit — matches the previous Rust behaviour).

if not (smelt.prompt and smelt.prompt.completer) then return end

local function commands_with_args()
  local out = {}
  for _, c in ipairs(smelt.cmd.list()) do
    if not c.hidden and c.args and #c.args > 0 then
      out[#out + 1] = c
    end
  end
  return out
end

local function find_arg_zone(text, cpos)
  if text:find("\n", 1, true) then return nil end
  for _, c in ipairs(commands_with_args()) do
    local prefix = "/" .. c.name .. " "
    if text:sub(1, #prefix) == prefix and cpos >= #prefix then
      return #prefix, c
    end
  end
  return nil
end

smelt.prompt.completer({
  detect = function(text, cpos)
    local anchor = find_arg_zone(text, cpos)
    return anchor
  end,
  items = function(_, text)
    local _, cmd = find_arg_zone(text, smelt.prompt.cursor())
    if not cmd then return {} end
    local out = {}
    for _, a in ipairs(cmd.args) do out[#out + 1] = { label = a } end
    return out
  end,
  query = function(text, anchor, cpos)
    if cpos <= anchor then return "" end
    return text:sub(anchor + 1, cpos)
  end,
  accept = function(item, anchor, _)
    local cpos = smelt.prompt.cursor()
    smelt.prompt.replace_range(anchor, cpos, item.label)
  end,
})
