-- `@file` completer - opens when the cursor sits inside an `@…` zone (an `@`
-- followed by non-whitespace bytes, or just `@` itself). Selecting an entry
-- inserts `@path ` (or `@"quoted path" ` when the label contains a space).

if not (smelt.prompt and smelt.prompt.register_completer and smelt.files) then return end

local path_format = require("smelt.completers.path_format")
local file_row = require("smelt.widgets.file_row")

-- Only the token at the cursor can contain an active `@` anchor. Include
-- the byte after the cursor so cursor-on-`@` also matches.
local function cursor_in_at_zone(buf, cpos)
  cpos = math.min(cpos, #buf)
  for i = math.min(#buf, cpos + 1), 1, -1 do
    local ch = buf:sub(i, i)
    if ch == "@" then
      if i == 1 or buf:sub(i - 1, i - 1):match("%s") then return i - 1 end
      return nil
    end
    if i <= cpos and ch:match("%s") then return nil end
  end
  return nil
end

local function query_at(anchor, text, cpos)
  if cpos > anchor + 1 then return text:sub(anchor + 2, cpos) end
  return ""
end

smelt.prompt.register_completer({
  limit = 200,
  render = function(item) return file_row.picker(item) end,
  detect = function(text, cpos)
    return cursor_in_at_zone(text, cpos)
  end,
  matches = function(anchor, text, cpos, limit)
    return smelt.files.search(query_at(anchor, text, cpos), {
      limit = limit,
      include_dirs = true,
    })
  end,
  query = function(text, anchor, cpos)
    return query_at(anchor, text, cpos)
  end,
  accept = function(item, anchor, _)
    if item._synthetic then return end
    -- Replace from `@` through the cursor with the quoted token + trailing space.
    local cpos = smelt.prompt.cursor()
    smelt.files.accept(item)
    smelt.prompt.replace_range(anchor, cpos, path_format.quote_at(path_format.item_path(item)) .. " ")
  end,
})
