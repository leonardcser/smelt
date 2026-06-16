-- Built-in edit_file tool. Exact-string find/replace under flock + mtime staleness check.

local transcript_defaults = require("smelt.transcript.defaults")

local function replace_first(haystack, needle, replacement)
  local s, e = string.find(haystack, needle, 1, true)
  if not s then
    return haystack
  end
  return haystack:sub(1, s - 1) .. replacement .. haystack:sub(e + 1)
end

local function replace_all(haystack, needle, replacement)
  local out = {}
  local start = 1
  while true do
    local s, e = string.find(haystack, needle, start, true)
    if not s then
      out[#out + 1] = haystack:sub(start)
      break
    end
    out[#out + 1] = haystack:sub(start, s - 1)
    out[#out + 1] = replacement
    start = e + 1
  end
  return table.concat(out)
end

local function apply_edit(content, old_string, new_string, do_all)
  if do_all then
    return replace_all(content, old_string, new_string)
  end
  return replace_first(content, old_string, new_string)
end

local function edit_fields(args)
  return args.file_path or "", args.old_string or "", args.new_string or "", args.replace_all == true
end

local function planned_diff(args)
  local path, old_string, new_string, do_all = edit_fields(args)
  local cached = path ~= "" and smelt.fs.file_state.get(path) or nil
  local content = cached and cached.content or nil
  if not content then
    return smelt.layout.diff({
      old = old_string,
      new = new_string,
      path = path,
      anchor = old_string,
    })
  end

  return smelt.layout.diff({
    old = content,
    new = apply_edit(content, old_string, new_string, do_all),
    path = path,
    anchor = old_string,
  })
end

local function line_delta_detail(old_text, new_text)
  local removed = smelt.text.line_count(old_text or "")
  local added = smelt.text.line_count(new_text or "")
  return tostring(removed) .. " removed, " .. tostring(added) .. " added"
end

transcript_defaults.__tool_body_renderers.edit_file = function(block)
  local args = block.args or {}
  local meta = block.output and block.output.metadata
  if meta then
    return smelt.layout.diff({
      old = meta.old_content or args.old_string or "",
      new = meta.new_content or args.new_string or "",
      path = meta.path or args.file_path or "",
      anchor = args.old_string or "",
    })
  end
  return planned_diff(args)
end

transcript_defaults.__tool_collapsed_details.edit_file = function(block)
  local args = block.args or {}
  local meta = block.output and block.output.metadata
  if meta then
    return line_delta_detail(meta.old_content or args.old_string or "", meta.new_content or args.new_string or "")
  end
  return line_delta_detail(args.old_string or "", args.new_string or "")
end

smelt.tools.register({
  name = "edit_file",
  description = "Performs exact string replacements in files. The old_string must be unique in the file unless replace_all is true.",
  override = true,
  permission_defaults = { apply = "allow" },
  execution_mode = "sequential",
  effect = "write",
  parameters = {
    type = "object",
    properties = {
      file_path = {
        type = "string",
        description = "The absolute path to the file to modify",
      },
      old_string = {
        type = "string",
        description = "The text to replace",
      },
      new_string = {
        type = "string",
        description = "The text to replace it with (must be different from old_string)",
      },
      replace_all = {
        type = "boolean",
        description = "Replace all occurrences of old_string (default false)",
      },
    },
    required = { "file_path", "old_string", "new_string" },
  },
  summary = function(args)
    return smelt.path.display(args.file_path or "")
  end,
  preflight = function(args)
    local path = args.file_path or ""
    if path == "" or smelt.fs.file_state.has(path) then
      return nil
    end
    return "You must use read_file before editing. Read the file first."
  end,
  paths_for_workspace = function(args)
    local p = args.file_path or ""
    return p ~= "" and { { path = p, kind = "file" } } or {}
  end,
  preview = function(args)
    return planned_diff(args)
  end,

  execute = function(args)
    local path, old_string, new_string, do_all = edit_fields(args)

    if path == "" then
      return { content = "missing required parameter: file_path", is_error = true }
    end
    if smelt.notebook.is_notebook_path(path) then
      return {
        content = "Cannot use edit_file on a Jupyter notebook. Use edit_notebook instead.",
        is_error = true,
      }
    end

    local result = smelt.task.external(function(id)
      smelt.fs.__start_edit_file(id, path, old_string, new_string, do_all)
    end)
    if result.err then
      return { content = result.err, is_error = true }
    end

    return {
      content = string.format("edited %s", smelt.path.display(path)),
      metadata = {
        old_content = result.old_content or "",
        new_content = result.new_content or "",
        path = path,
      },
    }
  end,
})
