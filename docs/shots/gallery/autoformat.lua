-- Formats every file the agent edits, the moment the edit lands: a hook on finished tool calls runs
-- the project's formatter on the file and says so.
local FORMATTERS = {
  rs = { "rustfmt", { "--edition", "2021" } },
  py = { "ruff", { "format" } },
  go = { "gofmt", { "-w" } },
  ts = { "prettier", { "--write" } },
  lua = { "stylua", {} },
}
local pending = {}

smelt.events.on("tool_start", function(e)
  if e.tool == "edit_file" or e.tool == "write_file" then
    pending[#pending + 1] = e.args and (e.args.file_path or e.args.path)
  end
end)

smelt.events.on("tool_end", function(e)
  if e.tool ~= "edit_file" and e.tool ~= "write_file" then return end
  local path = table.remove(pending, 1)
  local fmt = path and FORMATTERS[path:match("%.(%w+)$") or ""]
  if not fmt or e.is_error then return end
  smelt.spawn(function()
    local args = { table.unpack(fmt[2]) }
    args[#args + 1] = path
    local res = smelt.process.run(fmt[1], args)
    local name = smelt.path.display(path)
    if res and res.exit_code == 0 then
      smelt.notify.info(fmt[1] .. " formatted " .. name)
    else
      smelt.notify.warn(fmt[1] .. " could not format " .. name)
    end
  end)
end)
