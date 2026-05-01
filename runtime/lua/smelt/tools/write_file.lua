-- Built-in write_file tool — overwrite (or create) a file with new
-- contents. Refuses to clobber a file that hasn't been read this
-- session and detects mid-flight mtime drift; both checks compose
-- the shared `engine::tools::FileStateCache` exposed at
-- `smelt.fs.file_state.*`. Notebooks are routed to `edit_notebook`
-- instead.

local UNREAD_OVERWRITE_ERR = "File already exists. Use edit_file to modify existing files, or read_file then write_file to replace."

local function preflight_message(file_path)
  if file_path == "" then
    return nil
  end
  if not smelt.fs.exists(file_path) then
    return nil
  end
  if not smelt.fs.file_state.has(file_path) then
    return UNREAD_OVERWRITE_ERR
  end
  return smelt.fs.file_state.staleness_error(file_path, "file")
end

smelt.tools.register({
  name = "write_file",
  description = "Writes a file to the local filesystem. This tool will overwrite the existing file if there is one at the provided path.",
  override = true,
  parameters = {
    type = "object",
    properties = {
      file_path = {
        type = "string",
        description = "The absolute path to the file to write (must be absolute, not relative)",
      },
      content = {
        type = "string",
        description = "The content to write to the file",
      },
    },
    required = { "file_path", "content" },
  },
  needs_confirm = function(args)
    return smelt.path.display(args.file_path or "")
  end,
  preflight = function(args)
    return preflight_message(args.file_path or "")
  end,
  execute = function(args)
    local path = args.file_path or ""
    local content = args.content or ""

    if path == "" then
      return { content = "missing required parameter: file_path", is_error = true }
    end
    if smelt.notebook.is_notebook_path(path) then
      return {
        content = "Cannot use write_file on a Jupyter notebook. Use edit_notebook instead.",
        is_error = true,
      }
    end

    local exists = smelt.fs.exists(path)
    local lock = nil
    if exists then
      if not smelt.fs.file_state.has(path) then
        return { content = UNREAD_OVERWRITE_ERR, is_error = true }
      end
      local stale = smelt.fs.file_state.staleness_error(path, "file")
      if stale then
        return { content = stale, is_error = true }
      end
      local guard, err = smelt.fs.try_flock(path)
      if not guard then
        return { content = err or "could not lock file", is_error = true }
      end
      lock = guard
    end

    local parent = smelt.path.parent(path)
    if parent and parent ~= "" then
      local _, mkdir_err = smelt.fs.mkdir_all(parent)
      if mkdir_err then
        return { content = mkdir_err, is_error = true }
      end
    end

    local len = #content
    local _, write_err = smelt.fs.write(path, content)
    if write_err then
      if lock then lock:release() end
      return { content = write_err, is_error = true }
    end

    smelt.fs.file_state.record_write(path, content)
    if lock then lock:release() end

    return string.format("wrote %d bytes to %s", len, smelt.path.display(path))
  end,
})
