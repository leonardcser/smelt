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

local function diff_from_content(path, old_content, new_content, anchor)
  return smelt.layout.diff({
    old = old_content,
    new = new_content,
    path = path,
    anchor = anchor,
    full_file = true,
  })
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

  return diff_from_content(path, content, apply_edit(content, old_string, new_string, do_all), old_string)
end

local function planned_output(args)
  local path, old_string, new_string, do_all = edit_fields(args)
  if old_string == "" and new_string == "" then
    return nil
  end

  local cached = path ~= "" and old_string ~= "" and smelt.fs.file_state.get(path) or nil
  local content = cached and cached.content or nil
  if not content then
    return nil
  end

  return {
    content = "",
    is_error = false,
    metadata = {
      path = path,
      old_content = content,
      new_content = apply_edit(content, old_string, new_string, do_all),
    },
  }
end

local function line_label(count, label)
  return tostring(count) .. " " .. label .. (count == 1 and "" or "s")
end

local function replacement_line_detail(old_text, new_text)
  local old_lines = smelt.text.line_count(old_text or "")
  local new_lines = smelt.text.line_count(new_text or "")
  return line_label(old_lines, "old line") .. ", " .. line_label(new_lines, "new line")
end

local function draft_preview(args, block)
  if not (block and block.draft_finished) then
    return nil
  end

  local path, old_string, new_string, do_all = edit_fields(args)
  if old_string == "" and new_string == "" then
    return nil
  end

  local cached = path ~= "" and old_string ~= "" and smelt.fs.file_state.get(path) or nil
  local content = cached and cached.content or nil
  if content then
    return diff_from_content(path, content, apply_edit(content, old_string, new_string, do_all), old_string)
  end

  return nil
end

smelt.transcript.register_tool("edit_file", {
  cache_key = "smelt.tool-presentation.edit_file:v1",
  body = function(block, ctx, opts)
    if block.output and block.output.is_error then
      return transcript_defaults.render_tool_output_tail(block.output, ctx, opts)
    end
    local args = block.args or {}
    local meta = (block.output and block.output.metadata) or (block.preview_output and block.preview_output.metadata)
    if not meta then return nil end
    return diff_from_content(
      meta.path or args.file_path or "",
      meta.old_content or args.old_string or "",
      meta.new_content or args.new_string or "",
      args.old_string or ""
    )
  end,
  draft = function(block)
    return draft_preview(block.args or {}, block)
  end,
  compact = function(block, ctx)
    if block.output and block.output.is_error then
      return transcript_defaults.render_tool_output_tail(block.output, ctx, {
        rows = (ctx and ctx.limits and ctx.limits.collapsed_error_rows) or 4,
        keep = "head",
        marker = "below",
      })
    end
    local args = block.args or {}
    return replacement_line_detail(args.old_string or "", args.new_string or "")
  end,
})

smelt.tools.register({
  name = "edit_file",
  description = "Perform exact string replacements in files. By default, old_string must be unique; set replace_all to replace every match.",
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
        description = "Replacement text. Must differ from old_string.",
      },
      replace_all = {
        type = "boolean",
        description = "Replace every old_string match. Defaults to false.",
      },
    },
    required = { "file_path", "old_string", "new_string" },
  },
  summary = function(args, ctx)
    return smelt.tools.path_summary(args.file_path or "", ctx)
  end,
  preflight = function(args)
    local path = args.file_path or ""
    if path == "" or smelt.fs.file_state.has(path) then
      return nil
    end
    return "read the file with read_file before editing"
  end,
  paths_for_workspace = function(args)
    local p = args.file_path or ""
    return p ~= "" and { { path = p, kind = "file" } } or {}
  end,
  preview = function(args)
    return planned_diff(args)
  end,
  preview_output = function(args)
    return planned_output(args or {})
  end,
  execute = function(args)
    local path, old_string, new_string, do_all = edit_fields(args)

    if path == "" then
      return { content = "missing required parameter: file_path", is_error = true }
    end
    if smelt.notebook.is_notebook_path(path) then
      return {
        content = "cannot use edit_file on a Jupyter notebook; use edit_notebook instead",
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
