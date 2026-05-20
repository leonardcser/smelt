-- `@file` completer — opens when the cursor sits inside an `@…` zone (an `@`
-- followed by non-whitespace bytes, or just `@` itself). Selecting an entry
-- inserts `@path ` (or `@"quoted path" ` when the label contains a space).

if not (smelt.prompt and smelt.prompt.completer) then return end

-- Returns the byte offset of the `@` anchor when the cursor sits inside an
-- `@…` zone. Mirrors the previous Rust `cursor_in_at_zone` helper: `@` must
-- be preceded by whitespace or the buffer start, and the bytes between the
-- anchor and the cursor must not contain whitespace.
local function cursor_in_at_zone(buf, cpos)
  if cpos > #buf then cpos = #buf end
  -- Include one byte past the cursor so cursor-on-`@` is matched.
  local search_end = math.min(#buf, cpos + 1)
  local at_pos = buf:sub(1, search_end):find("@[^@]*$")
  if not at_pos then return nil end
  -- 1-based Lua index → 0-based byte offset.
  local at_byte = at_pos - 1
  if at_byte > 0 then
    local prev = buf:sub(at_byte, at_byte)
    if not prev:match("%s") then return nil end
  end
  if at_byte < cpos then
    local between = buf:sub(at_byte + 2, cpos)
    if between:find("%s") then return nil end
  end
  return at_byte
end

local function quote_if_needed(label)
  if label:find(" ", 1, true) then
    return '@"' .. label .. '"'
  end
  return "@" .. label
end

smelt.prompt.completer({
  prefix = "./",
  detect = function(text, cpos)
    return cursor_in_at_zone(text, cpos)
  end,
  items = function()
    local out = {}
    for _, path in ipairs(smelt.fs.workspace_files()) do
      out[#out + 1] = { label = path }
    end
    return out
  end,
  query = function(text, anchor, cpos)
    -- Skip the `@` byte.
    if cpos <= anchor + 1 then return "" end
    return text:sub(anchor + 2, cpos)
  end,
  accept = function(item, anchor, _)
    -- Replace from `@` through the cursor with the quoted token + trailing space.
    local cpos = smelt.prompt.cursor()
    smelt.prompt.replace_range(anchor, cpos, quote_if_needed(item.label) .. " ")
  end,
})
