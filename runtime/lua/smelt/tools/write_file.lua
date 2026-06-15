-- Built-in write_file tool. Refuses to overwrite unread files and detects mtime drift.

local transcript_defaults = require("smelt.transcript.defaults")

local function file_view(args)
  return smelt.layout.file_view({
    content = args.content or "",
    path    = args.file_path or "",
  })
end

transcript_defaults.__tool_body_renderers.write_file = function(block)
  return file_view(block.args or {})
end

smelt.tools.register({
  name = "write_file",
  description = "Writes a file to the local filesystem. This tool will overwrite the existing file if there is one at the provided path.",
  override = true,
  permission_defaults = { apply = "allow" },
  effect = "write",
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
  summary = function(args)
    return smelt.path.display(args.file_path or "")
  end,
  paths_for_workspace = function(args)
    local p = args.file_path or ""
    return p ~= "" and { { path = p, kind = "file" } } or {}
  end,
  preview = function(args)
    return file_view(args)
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

    local result = smelt.task.external(function(id)
      smelt.fs.__start_write_file(id, path, content)
    end)
    if result.err then
      return { content = result.err, is_error = true }
    end

    return string.format("wrote %d bytes to %s", result.bytes or #content, smelt.path.display(path))
  end,
})
