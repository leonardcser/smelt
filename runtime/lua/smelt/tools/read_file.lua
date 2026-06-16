-- Built-in read_file tool. Supports text, notebooks (.ipynb), and images.
-- Returns a stub for unchanged files at the same range to save prompt-cache tokens.

local transcript_defaults = require("smelt.transcript.defaults")

local DEFAULT_LINE_LIMIT = 2000

local FILE_UNCHANGED_STUB = "File unchanged since last read. The content from the earlier read_file "
  .. "tool_result in this conversation is still current - refer to that "
  .. "instead of re-reading."

local function effective_range(args)
  local offset_raw = tonumber(args.offset) or 0
  local offset = math.max(1, math.floor(offset_raw))
  local limit_raw = tonumber(args.limit) or 0
  local limit = limit_raw > 0 and math.floor(limit_raw) or DEFAULT_LINE_LIMIT
  return offset, limit
end

local function cached_read_range(path, offset, limit)
  local cached = smelt.fs.file_state.get(path)
  if not cached or not cached.read_range then
    return nil
  end
  if cached.read_range.offset == offset and cached.read_range.limit == limit then
    return cached
  end
  return nil
end

local function format_text_window(content, offset, limit)
  local lines = {}
  for line in (content .. "\n"):gmatch("([^\n]*)\n") do
    lines[#lines + 1] = line
  end
  -- gmatch leaves a trailing empty entry on a trailing newline; drop it.
  if content:sub(-1) == "\n" and lines[#lines] == "" then
    lines[#lines] = nil
  end
  local total = #lines
  local start_idx = offset
  if start_idx > total then
    return nil
  end
  local end_idx = math.min(start_idx + limit - 1, total)
  local out = {}
  for i = start_idx, end_idx do
    local line = lines[i] or ""
    if #line > 2000 then
      line = line:sub(1, 2000)
    end
    out[#out + 1] = string.format("%4d\t%s", i, line)
  end
  return table.concat(out, "\n")
end

transcript_defaults.__tool_body_renderers.read_file = function(block, ctx)
  if not block.output then return nil end
  return transcript_defaults.render_tool_output_tail(block.output, ctx)
end

transcript_defaults.__tool_collapsed_details.read_file = function(block)
  return smelt.text.line_count((block.output and block.output.content) or "") .. " lines"
end

smelt.tools.register(smelt.tools._with_watchdog({
  name = "read_file",
  description = "Reads a file from the local filesystem. Supports text files and image files (png, jpg, gif, webp, bmp, tiff, svg).",
  override = true,
  permission_defaults = { normal = "allow", plan = "allow", apply = "allow" },
  effect = "read",
  parameters = {
    type = "object",
    properties = {
      file_path = {
        type = "string",
        description = "The absolute path to the file to read",
      },
      offset = {
        type = "integer",
        description = "The line number to start reading from (1-based). Only provide if the file is too large to read at once.",
      },
      limit = {
        type = "integer",
        description = "The number of lines to read. Only provide if the file is too large to read at once.",
      },
      timeout_ms = {
        type = "integer",
        description = "Timeout in milliseconds (default: 15000)",
      },
    },
    required = { "file_path" },
  },
  summary = function(args)
    return smelt.path.display(args.file_path or "")
  end,
  paths_for_workspace = function(args)
    local p = args.file_path or ""
    return p ~= "" and { { path = p, kind = "file" } } or {}
  end,
  execute = function(args)
    local path = args.file_path or ""
    if path == "" then
      return { content = "missing required parameter: file_path", is_error = true }
    end

    if smelt.image.is_image_file(path) then
      local data_url, err = smelt.image.read_as_data_url_async(path)
      if not data_url then
        return { content = err or "could not read image", is_error = true }
      end
      return string.format("![image](%s)", data_url)
    end

    local offset, limit = effective_range(args)
    local cached = cached_read_range(path, offset, limit)

    if smelt.notebook.is_notebook_path(path) then
      local rendered, render_err, raw, mtime_ms = smelt.notebook.read_async(path, offset, limit)
      if not rendered then
        return { content = render_err or "could not render notebook", is_error = true }
      end
      if cached and mtime_ms and cached.mtime_ms == mtime_ms then
        return FILE_UNCHANGED_STUB
      end
      if raw and mtime_ms then
        smelt.fs.file_state.record_read_with_mtime(path, raw, offset, limit, mtime_ms)
      elseif raw then
        smelt.fs.file_state.record_read(path, raw, offset, limit)
      end
      return rendered
    end

    local content, read_err, mtime_ms = smelt.fs.read_async(path)
    if not content then
      return { content = read_err or "could not read file", is_error = true }
    end
    if cached and mtime_ms and cached.mtime_ms == mtime_ms then
      return FILE_UNCHANGED_STUB
    end

    local formatted = format_text_window(content, offset, limit)
    if mtime_ms then
      smelt.fs.file_state.record_read_with_mtime(path, content, offset, limit, mtime_ms)
    else
      smelt.fs.file_state.record_read(path, content, offset, limit)
    end
    if formatted == nil then
      return "offset beyond end of file"
    end
    return formatted
  end,
}, { default_ms = 15000, max_ms = 60000 }))
