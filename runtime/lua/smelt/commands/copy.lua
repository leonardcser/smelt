-- Built-in /copy command. Copies recent conversation messages to the system clipboard.

local notify_handle = nil

local function get_notify()
  if notify_handle then return notify_handle end
  if smelt.notify and smelt.notify.scoped then
    notify_handle = smelt.notify.scoped("copy")
    return notify_handle
  end
  notify_handle = setmetatable({
    error = function(msg)
      if smelt.notify and smelt.notify.error then smelt.notify.error(msg, "copy") end
    end,
  }, {
    __call = function(_, msg)
      if smelt.notify then smelt.notify(msg, "copy") end
    end,
  })
  return notify_handle
end

local function usage()
  return "usage: /copy [--role user|assistant] [--headers] [N]"
end

local function parse_args(arg)
  local opts = { count = 1, role = nil, headers = false }
  local tokens = {}
  for token in tostring(arg or ""):gmatch("%S+") do
    tokens[#tokens + 1] = token
  end

  local i = 1
  while i <= #tokens do
    local token = tokens[i]
    if token == "--headers" then
      opts.headers = true
    elseif token == "--role" or token == "-r" then
      i = i + 1
      local role = tokens[i]
      if role ~= "user" and role ~= "assistant" then
        return nil, usage()
      end
      opts.role = role
    elseif token:match("^%-%-role=") then
      local role = token:match("^%-%-role=(.*)$")
      if role ~= "user" and role ~= "assistant" then
        return nil, usage()
      end
      opts.role = role
    elseif token:match("^%d+$") then
      local count = tonumber(token)
      if not count or count < 1 then
        return nil, usage()
      end
      opts.count = count
    else
      return nil, usage()
    end
    i = i + 1
  end

  return opts, nil
end

local function select_messages(opts)
  if not (smelt.session and smelt.session.conversation) then
    return nil, "/copy requires an interactive session"
  end
  local rows = smelt.session.conversation()
  local selected = {}
  for i = #rows, 1, -1 do
    local row = rows[i]
    local content = row and row.content or ""
    if content ~= "" and (not opts.role or row.role == opts.role) then
      selected[#selected + 1] = row
      if #selected >= opts.count then break end
    end
  end

  local out = {}
  for i = #selected, 1, -1 do
    out[#out + 1] = selected[i]
  end
  return out
end

local function role_label(role)
  if role == "user" then return "User" end
  if role == "assistant" then return "Assistant" end
  return tostring(role or "Message")
end

local function format_messages(rows, headers)
  if #rows == 1 and not headers then
    return rows[1].content or ""
  end

  local parts = {}
  for _, row in ipairs(rows) do
    parts[#parts + 1] = role_label(row.role) .. ":\n" .. (row.content or "")
  end
  return table.concat(parts, "\n\n")
end

local function copy_messages(arg)
  local opts, err = parse_args(arg)
  if not opts then
    get_notify().error(err)
    return
  end

  local rows, select_err = select_messages(opts)
  if not rows then
    get_notify().error(select_err)
    return
  end
  if #rows == 0 then
    if opts.role then
      get_notify().error("no " .. opts.role .. " messages to copy")
    else
      get_notify().error("no messages to copy")
    end
    return
  end

  local text = format_messages(rows, opts.headers)
  if not (smelt.clipboard and smelt.clipboard.write) then
    get_notify().error("clipboard is unavailable")
    return
  end
  local ok, write_err = pcall(smelt.clipboard.write, text)
  if not ok then
    get_notify().error("clipboard write failed: " .. tostring(write_err))
    return
  end

  local suffix = #rows == 1 and "message" or "messages"
  get_notify()("copied " .. tostring(#rows) .. " " .. suffix)
end

smelt.cmd.register("copy", copy_messages, {
  desc = "copy recent conversation message(s) to clipboard",
  args = { "[--role user|assistant]", "[--headers]", "[N]" },
})

smelt.cmd.register("yank", copy_messages, {
  desc = "alias for /copy",
  args = { "[--role user|assistant]", "[--headers]", "[N]" },
})

return {
  parse_args = parse_args,
  select_messages = select_messages,
  format_messages = format_messages,
}
