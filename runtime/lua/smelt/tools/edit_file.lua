-- Built-in edit_file tool. Exact-string find/replace under flock + mtime staleness check.

local transcript_defaults = require("smelt.transcript.defaults")

local function edit_fields(args)
  return args.file_path or "", args.old_string or "", args.new_string or "", args.replace_all == true
end

local function plan_edit(args)
  return smelt.fs.__plan_edit_file(edit_fields(args))
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

local function argument_field(block, name)
  for _, field in ipairs(block.argument_fields or {}) do
    if field.name == name then return field end
  end
  return nil
end

local function retained_diff(old_content, new_content, path, anchor, full_file)
  if not (old_content and old_content.content_id and new_content and new_content.content_id) then
    return nil
  end
  return smelt.layout.content_diff(old_content.content_id, new_content.content_id, {
    anchor_content_id = anchor and anchor.content_id or nil,
    path = path or "",
    full_file = full_file == true,
  })
end

local function planned_diff(args)
  local path, old_string = edit_fields(args)
  local plan = plan_edit(args)
  if plan.err then
    return nil
  end
  return diff_from_content(path, plan.old_content, plan.new_content, old_string)
end

local function planned_output(args)
  local plan = plan_edit(args)
  if plan.err then
    return nil
  end

  return {
    content = "",
    is_error = false,
    metadata = {
      path = args.file_path or "",
    },
    display_content = {
      old_content = plan.old_content,
      new_content = plan.new_content,
    },
  }
end

local function line_label(count, label)
  return tostring(count) .. " " .. label .. (count == 1 and "" or "s")
end

local function replacement_line_detail(block)
  local args = block.args or {}
  local old = argument_field(block, "old_string")
  local new = argument_field(block, "new_string")
  local old_lines = (old and old.content_lines) or smelt.text.line_count(args.old_string or "")
  local new_lines = (new and new.content_lines) or smelt.text.line_count(args.new_string or "")
  return line_label(old_lines, "old line") .. ", " .. line_label(new_lines, "new line")
end

local function draft_preview(args, block)
  if not (block and block.draft_finished) then
    return nil
  end
  if smelt.fs.__validate_edit_file(edit_fields(args)) then
    return nil
  end

  return retained_diff(
    argument_field(block, "old_string"),
    argument_field(block, "new_string"),
    args.file_path or "",
    argument_field(block, "old_string"),
    false
  )
end

smelt.transcript.register_tool("edit_file", {
  cache_key = "smelt.tool-presentation.edit_file:v2",
  body = function(block, ctx, opts)
    if block.output and block.output.is_error then
      return transcript_defaults.render_tool_output_tail(block.output, ctx, opts)
    end
    local args = block.args or {}
    local output = block.output or block.preview_output
    local fields = output and output.content_fields
    local meta = output and output.metadata
    if not fields then return nil end
    return retained_diff(
      fields.old_content,
      fields.new_content,
      (meta and meta.path) or args.file_path or "",
      argument_field(block, "old_string"),
      true
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
    return replacement_line_detail(block)
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
    return smelt.fs.__validate_edit_file(edit_fields(args))
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

    local result = smelt.task.external(function(id)
      smelt.fs.__start_edit_file(id, path, old_string, new_string, do_all)
    end)
    if result.err then
      return { content = result.err, is_error = true }
    end

    return {
      content = string.format("edited %s", smelt.path.display(path)),
      metadata = {
        path = path,
      },
      display_content = {
        old_content = result.old_content or "",
        new_content = result.new_content or "",
      },
    }
  end,
})
