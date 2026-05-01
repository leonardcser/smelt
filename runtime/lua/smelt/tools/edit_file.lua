-- Built-in edit_file tool — exact-string find/replace under a flock,
-- guarded by the shared `engine::tools::FileStateCache` mtime check
-- exposed via `smelt.fs.file_state.*`. Notebooks redirect to
-- `edit_notebook`.

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

smelt.tools.register({
  name = "edit_file",
  description = "Performs exact string replacements in files. The old_string must be unique in the file unless replace_all is true.",
  override = true,
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
  needs_confirm = function(args)
    return smelt.path.display(args.file_path or "")
  end,
  preflight = function(args)
    local path = args.file_path or ""
    if path == "" then
      return nil
    end
    return smelt.fs.file_state.staleness_error(path, "file")
  end,
  execute = function(args)
    local path = args.file_path or ""
    local old_string = args.old_string or ""
    local new_string = args.new_string or ""
    local do_all = args.replace_all == true

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
          "old_string found %d times — must be unique, or set replace_all to true",
          count
        ),
        is_error = true,
      }
    end

    local new_content
    if do_all then
      new_content = replace_all(content, old_string, new_string)
    else
      new_content = replace_first(content, old_string, new_string)
    end

    local _, write_err = smelt.fs.write(path, new_content)
    if write_err then
      lock:release()
      return { content = write_err, is_error = true }
    end

    smelt.fs.file_state.record_write(path, new_content)
    lock:release()

    return string.format("edited %s", smelt.path.display(path))
  end,
})
