-- Built-in /copy command. Copies recent conversation messages to the system clipboard.


local notify_handle = nil

local function get_notify()
  if notify_handle then return notify_handle end
  if smelt.notify and smelt.notify.scoped then
    notify_handle = smelt.notify.scoped("copy")
    return notify_handle
  end
  notify_handle = {
    info = function(msg)
      if smelt.notify and smelt.notify.info then smelt.notify.info(msg, "copy") end
    end,
    error = function(msg)
      if smelt.notify and smelt.notify.error then smelt.notify.error(msg, "copy") end
    end,
  }
  return notify_handle
end

local function usage()
  return "usage: /copy [--role user|assistant] [--headers] [N]"
end

local SPEC = {
  args = "N",
  flags = { { name = "role", short = "r", kind = "string" }, { name = "headers", kind = "boolean" } },
}

local function parse_args(args, flags)
  if #args > 1 or not (flags.role == nil or flags.role == "user" or flags.role == "assistant") then
    return nil, usage()
  end
  local count = 1
  if args[1] then
    count = args[1]:match("^%d+$") and tonumber(args[1])
    if not count or count < 1 then return nil, usage() end
  end
  return { count = count, role = flags.role, headers = flags.headers }, nil
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

local function copy_messages(_, args, flags)
  local opts, err = parse_args(args, flags)
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
  get_notify().info("copied " .. tostring(#rows) .. " " .. suffix)
end

smelt.cmd.register("copy", copy_messages, {
  desc = "copy recent conversation message(s) to clipboard",
  args = { "[--role user|assistant]", "[--headers]", "[N]" },
  spec = SPEC,
})

smelt.cmd.register("yank", copy_messages, {
  desc = "alias for /copy",
  args = { "[--role user|assistant]", "[--headers]", "[N]" },
  spec = SPEC,
})

return {
  parse_args = parse_args,
  select_messages = select_messages,
  format_messages = format_messages,
}
