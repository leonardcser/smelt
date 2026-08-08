-- Built-in read_file tool. Supports text, notebooks (.ipynb), images, and PDFs.
-- Returns a stub for unchanged files at the same range to save prompt-cache tokens.

local transcript_defaults = require("smelt.transcript.defaults")

local DEFAULT_LINE_LIMIT = 2000

local FILE_UNCHANGED_STUB = "file unchanged since last read; use the earlier read_file "
  .. "tool_result instead of re-reading"

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

local function line_count(content)
  if content == nil or content == "" then return 0 end
  return smelt.text.line_count(content)
end

local function range_display(args, content)
  args = args or {}
  local has_offset = args.offset ~= nil
  local has_limit = args.limit ~= nil
  local offset = math.max(1, math.floor(tonumber(args.offset) or 1))
  local limit = tonumber(args.limit)
  if limit and limit > 0 then
    limit = math.floor(limit)
  else
    limit = DEFAULT_LINE_LIMIT
  end

  local reached_limit = content ~= nil and line_count(content) >= limit
  if not has_offset and not has_limit and not reached_limit then return "" end
  if limit == DEFAULT_LINE_LIMIT and not reached_limit then
    return has_offset and (":" .. tostring(offset)) or ""
  end
  return ":" .. tostring(offset) .. "-" .. tostring(offset + limit - 1)
end

local function is_pdf_file(path)
  return type(path) == "string" and path:lower():sub(-4) == ".pdf"
end

local function active_transport_supports_tool_results(modality)
  if not smelt.model or not smelt.model.transport then return false end
  local transport = smelt.model.transport()
  if not transport then return false end
  return transport[modality .. "_tool_results"] == true
end

local function active_model_supports(modality)
  if not smelt.model or not smelt.model.supports_input then return modality == "text" end
  return smelt.model.supports_input(modality)
end

local function multimodal_error(kind, path)
  if not active_model_supports(kind) then
    return {
      content = string.format("cannot read %s file %s: active model does not support %s input", kind, path, kind),
      is_error = true,
    }
  end
  if not active_transport_supports_tool_results(kind) then
    return {
      content = string.format("cannot read %s file %s: active provider transport cannot send %s tool results", kind, path, kind),
      is_error = true,
    }
  end
  return nil
end

local function multimodal_result(kind, path, mime, info)
  if not info then
    local info_err
    info, info_err = smelt.fs.file_info_async(path)
    if not info then
      return { content = info_err or ("could not read file: " .. path), is_error = true }
    end
  end
  local err = multimodal_error(kind, path)
  if err then return err end
  local data_url, read_err = smelt.image.read_as_data_url_async(path, mime)
  if not data_url then
    return { content = read_err or ("could not read file: " .. path), is_error = true }
  end
  mime = mime or data_url:match("^data:([^;,]+);base64,")
  if not mime then
    return { content = "could not determine file MIME type: " .. path, is_error = true }
  end
  return {
    content = string.format("%s file attached: %s", kind, path),
    is_error = false,
    metadata = {
      kind = "file_attachment",
      modality = kind,
      path = path,
      mime = mime,
      data_url = data_url,
      label = smelt.image.label_from_path and smelt.image.label_from_path(path) or path,
    },
  }
end

local function is_binary_read_error(err)
  err = tostring(err or ""):lower()
  return err:find("utf-8", 1, true) ~= nil or err:find("stream did not contain valid utf-8", 1, true) ~= nil
end

function smelt.tools.read_file_summary(args, content, ctx)
  args = args or {}
  return smelt.tools.path_summary(args.file_path or "", ctx, { suffix = range_display(args, content) })
end

smelt.transcript.register_tool("read_file", {
  cache_key = "smelt.tool-presentation.read_file:v1",
  body = function(block, ctx)
    if not block.output then return nil end
    return transcript_defaults.render_tool_output_tail(block.output, ctx)
  end,
  compact = function(block)
    return smelt.text.line_count((block.output and block.output.content) or "") .. " lines"
  end,
})

smelt.tools.register(smelt.tools._with_watchdog({
  name = "read_file",
  description = "Reads a file from the local filesystem. Supports text files, image files (png, jpg, gif, webp, bmp, tiff, svg), and PDFs when the active model/provider can accept them.",
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
  summary = function(args, ctx)
    return smelt.tools.read_file_summary(args or {}, nil, ctx)
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

    local file_info, file_info_err = smelt.fs.file_info_async(path)
    if not file_info then
      return { content = file_info_err or "could not read file", is_error = true }
    end

    if file_info.kind == "image" or smelt.image.is_image_file(path) then
      return multimodal_result("image", path, nil, file_info)
    end

    if file_info.kind == "pdf" or is_pdf_file(path) then
      return multimodal_result("pdf", path, "application/pdf", file_info)
    end

    if file_info.kind == "binary" then
      return { content = "cannot read binary file as text: " .. path, is_error = true }
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
      if is_binary_read_error(read_err) then
        return { content = "cannot read binary file as text: " .. path, is_error = true }
      end
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
