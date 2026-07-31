-- Built-in write_file tool. Refuses to overwrite unread files and detects mtime drift.

local transcript_defaults = require("smelt.transcript.defaults")

local function file_view(args)
  return smelt.layout.file_view({
    content = args.content or "",
    path    = args.file_path or "",
  })
end

smelt.transcript.register_tool("write_file", {
  cache_key = "smelt.tool-presentation.write_file:v1",
  body = function(block, ctx, opts)
    if block.output and block.output.is_error then
      return transcript_defaults.render_tool_output_tail(block.output, ctx, opts)
    end
    return file_view(block.args or {})
  end,
  draft = function(block)
    return file_view(block.args or {})
  end,
  compact = function(block, ctx)
    if block.output and block.output.is_error then
      return transcript_defaults.render_tool_output_tail(block.output, ctx, {
        rows = (ctx and ctx.limits and ctx.limits.collapsed_error_rows) or 4,
        keep = "head",
        marker = "below",
      })
    end
    return "wrote " .. smelt.text.line_count((block.args and block.args.content) or "") .. " lines"
  end,
})

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
  summary = function(args, ctx)
    return smelt.tools.path_summary(args.file_path or "", ctx)
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
        content = "cannot use write_file on a Jupyter notebook; use edit_notebook instead",
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
