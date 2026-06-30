-- Plain path completer - Tab completes the filesystem path token under the
-- cursor. It is manual-only and directory-scoped, matching shell completion
-- rather than workspace fuzzy search.

if not (smelt.prompt and smelt.prompt.completer and smelt.fs and smelt.path) then return end

local path_format = require("smelt.completers.path_format")

local function token_at(text, cpos)
  if cpos > #text then cpos = #text end
  local start = cpos
  while start > 0 do
    local ch = text:sub(start, start)
    if ch:match("%s") then break end
    start = start - 1
  end
  local token = text:sub(start + 1, cpos)
  if token == "" then return nil end
  return start, cpos, token
end

local function looks_like_path(token)
  if token:find("://", 1, true) then return false end
  return token:sub(1, 1) == "/"
      or token:sub(1, 2) == "~/"
      or token:sub(1, 2) == "./"
      or token:sub(1, 3) == "../"
      or token:find("/", 1, true) ~= nil
end

local function split_token(token)
  local slash = token:match("^.*()/")
  if slash then
    return token:sub(1, slash), token:sub(slash + 1)
  end
  return "", token
end

local function list_dir_path(dir_token)
  if dir_token == "" then return "." end
  if dir_token:sub(1, 1) == "~" then
    local ok, expanded = pcall(smelt.path.expand, dir_token)
    if ok then return expanded end
  end
  return dir_token
end

local function path_token(text, cpos)
  local start, finish, token = token_at(text, cpos)
  if not start or not looks_like_path(token) then return nil end
  local dir_token, prefix = split_token(token)
  return {
    start = start,
    finish = finish,
    token = token,
    dir_token = dir_token,
    dir_path = list_dir_path(dir_token),
    prefix = prefix,
  }
end

local function complete_path(token, limit)
  local result, err = smelt.fs.complete_path(token.dir_path, token.prefix, {
    limit = limit,
    insert_prefix = token.dir_token,
  })
  if result then
    path_format.mark_file_icon_rows(result)
    return result
  end
  return { items = {}, status = "empty", message = err or "no matches" }
end

local function query_at(anchor, text, cpos)
  local token = path_token(text, cpos)
  if not token or token.start ~= anchor then return nil end
  return token
end

smelt.prompt.completer({
  manual = true,
  auto = false,
  accept_single = true,
  limit = 200,
  detect = function(text, cpos)
    local token = path_token(text, cpos)
    return token and token.start or nil
  end,
  matches = function(anchor, text, cpos, limit)
    local token = query_at(anchor, text, cpos)
    if not token then return { items = {}, status = "empty" } end
    return complete_path(token, limit)
  end,
  query = function(text, anchor, cpos)
    local token = query_at(anchor, text, cpos)
    return token and token.token or ""
  end,
  accept = function(item, anchor, _)
    if item._synthetic then return end
    local cpos = smelt.prompt.cursor()
    local insert = path_format.item_path(item)
    if item.kind == "file" then insert = path_format.quote_path_token(insert) .. " " end
    smelt.prompt.replace_range(anchor, cpos, insert)
  end,
})
