-- Built-in read_file tool — text files, Jupyter notebooks (`.ipynb`),
-- and image files (png/jpg/gif/webp/bmp/tiff/svg). Caches each read
-- in `Core.files` (the shared `engine::tools::FileStateCache`) so
-- repeat reads of the same range against an unchanged file return a
-- short stub instead of the full content, saving prompt-cache tokens.

local DEFAULT_LINE_LIMIT = 2000

local FILE_UNCHANGED_STUB = "File unchanged since last read. The content from the earlier read_file "
  .. "tool_result in this conversation is still current — refer to that "
  .. "instead of re-reading."

local function effective_range(args)
  local offset_raw = tonumber(args.offset) or 0
  local offset = math.max(1, math.floor(offset_raw))
  local limit_raw = tonumber(args.limit) or 0
  local limit = limit_raw > 0 and math.floor(limit_raw) or DEFAULT_LINE_LIMIT
  return offset, limit
end

local function dedup_stub(path, offset, limit)
  local cached = smelt.fs.file_state.get(path)
  if not cached or not cached.read_range then
    return nil
  end
  if cached.read_range.offset ~= offset or cached.read_range.limit ~= limit then
    return nil
  end
  local current_mtime, _ = smelt.fs.file_state.mtime_ms(path)
  if not current_mtime then
    return nil
  end
  if current_mtime == cached.mtime_ms then
    return FILE_UNCHANGED_STUB
  end
  return nil
end

local function format_text_window(content, offset, limit)
  local lines = {}
  for line in (content .. "\n"):gmatch("([^\n]*)\n") do
    lines[#lines + 1] = line
  end
  -- string.gmatch above leaves a trailing empty entry when content ends in
  -- a newline; the engine impl uses `content.lines()` which drops it.
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

smelt.tools.register({
  name = "read_file",
  description = "Reads a file from the local filesystem. Supports text files and image files (png, jpg, gif, webp, bmp, tiff, svg).",
  override = true,
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
    },
    required = { "file_path" },
  },
  needs_confirm = function(args)
    return smelt.path.display(args.file_path or "")
  end,
  execute = function(args)
    local path = args.file_path or ""
    if path == "" then
      return { content = "missing required parameter: file_path", is_error = true }
    end

    if smelt.image.is_image_file(path) then
      local data_url, err = smelt.image.read_as_data_url(path)
      if not data_url then
        return { content = err or "could not read image", is_error = true }
      end
      return string.format("![image](%s)", data_url)
    end

    local offset, limit = effective_range(args)
    local stub = dedup_stub(path, offset, limit)
    if stub then
      return stub
    end

    if smelt.notebook.is_notebook_path(path) then
      local raw, raw_err = smelt.fs.read(path)
      if not raw then
        return { content = raw_err or "could not read notebook", is_error = true }
      end
      smelt.fs.file_state.record_read(path, raw, offset, limit)
      local rendered, render_err = smelt.notebook.read(path, offset, limit)
      if not rendered then
        return { content = render_err or "could not render notebook", is_error = true }
      end
      return rendered
    end

    local content, read_err = smelt.fs.read(path)
    if not content then
      return { content = read_err or "could not read file", is_error = true }
    end

    local formatted = format_text_window(content, offset, limit)
    smelt.fs.file_state.record_read(path, content, offset, limit)
    if formatted == nil then
      return "offset beyond end of file"
    end
    return formatted
  end,
})
