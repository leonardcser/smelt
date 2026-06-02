-- Built-in edit_file tool. Exact-string find/replace under flock + mtime staleness check.

local function count_occurrences(haystack, needle)
  if needle == "" then
    return 0
  end
  local count = 0
  local start = 1
  while true do
    local s, e = string.find(haystack, needle, start, true)
    if not s then
      break
    end
    count = count + 1
    start = e + 1
  end
  return count
end

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
  local content = path ~= "" and smelt.fs.read(path) or nil
  if not content then
    return smelt.layout.diff({
      old = old_string,
      new = new_string,
      path = path,
    })
  end

  return smelt.layout.diff({
    old = content,
    new = apply_edit(content, old_string, new_string, do_all),
    path = path,
    anchor = old_string,
  })
end

smelt.tools.register({
  name = "edit_file",
  description = "Performs exact string replacements in files. The old_string must be unique in the file unless replace_all is true.",
  override = true,
  permission_defaults = { apply = "allow" },
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
    if path == "" then
      return nil
    end
    return smelt.fs.file_state.staleness_error(path, "file")
  end,
  render = function(args, output, ctx)
    if output.is_error then
      return smelt.layout.tool_output(output, ctx)
    end
    local meta = output.metadata
    if meta then
      return smelt.layout.diff({
        old = meta.old_content or args.old_string or "",
        new = meta.new_content or args.new_string or "",
        path = meta.path or args.file_path or "",
        anchor = args.old_string or "",
      })
    end
    return planned_diff(args)
  end,
  paths_for_workspace = function(args)
    local p = args.file_path or ""
    return p ~= "" and { p } or {}
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

    local stale = smelt.fs.file_state.staleness_error(path, "file")
    if stale then
      return { content = stale, is_error = true }
    end

    local lock, lock_err = smelt.fs.try_flock(path)
    if not lock then
      return { content = lock_err or "could not lock file", is_error = true }
    end

    local content, read_err = smelt.fs.read(path)
    if not content then
      lock:release()
      return { content = read_err or "could not read file", is_error = true }
    end

    if old_string == new_string then
      lock:release()
      return { content = "old_string and new_string are identical", is_error = true }
    end

    local count = count_occurrences(content, old_string)
    if count == 0 then
      lock:release()
      return { content = "old_string not found in file", is_error = true }
    end
    if count > 1 and not do_all then
      lock:release()
      return {
        content = string.format(
          "old_string found %d times - must be unique, or set replace_all to true",
          count
        ),
        is_error = true,
      }
    end

    local new_content = apply_edit(content, old_string, new_string, do_all)

    local _, write_err = smelt.fs.write(path, new_content)
    if write_err then
      lock:release()
      return { content = write_err, is_error = true }
    end

    smelt.fs.file_state.record_write(path, new_content)
    lock:release()

    return {
      content = string.format("edited %s", smelt.path.display(path)),
      metadata = {
        old_content = content,
        new_content = new_content,
        path = path,
      },
    }
  end,
})
