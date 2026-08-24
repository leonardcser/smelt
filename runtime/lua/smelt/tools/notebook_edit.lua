-- Built-in edit_notebook tool. Replace/insert/delete a Jupyter cell with
-- staleness preflight and per-path flock.

local transcript_defaults = require("smelt.transcript.defaults")

local function preview_layout(meta)
  local lang = meta.syntax_ext
  local path = meta.path or ""
  local body
  if meta.edit_mode == "insert" then
    body = smelt.layout.file_view({
      content = meta.new_source or "",
      path    = path .. "." .. (lang or "py"),
      lang    = lang,
    })
  else
    body = smelt.layout.diff({
      old = meta.old_source or "",
      new = meta.new_source or "",
      path = lang and (path .. "." .. lang) or path,
      lang = lang,
    })
  end

  local title = meta.title or ""
  if title == "" then
    return body
  end
  return smelt.layout.vbox({ smelt.layout.text(title), body })
end

local function retained_layout(block)
  local output = block.output
  local fields = output and output.content_fields
  local meta = (output and output.metadata) or {}
  if not fields then return nil end

  local path = meta.path or ""
  local lang = meta.syntax_ext
  local body
  if meta.edit_mode == "insert" then
    local content = fields.new_source
    if not (content and content.content_id) then return nil end
    body = smelt.layout.content(content.content_id, {
      format = "file",
      path = path .. "." .. (lang or "py"),
      lang = lang,
    })
  else
    local old = fields.old_source
    local new = fields.new_source
    if not (old and old.content_id and new and new.content_id) then return nil end
    body = smelt.layout.content_diff(old.content_id, new.content_id, {
      path = lang and (path .. "." .. lang) or path,
      lang = lang,
      full_file = true,
    })
  end

  local title = meta.title or ""
  if title == "" then return body end
  return smelt.layout.vbox({ smelt.layout.text(title), body })
end

smelt.transcript.register_tool("edit_notebook", {
  cache_key = "smelt.tool-presentation.edit_notebook:v2",
  body = function(block)
    return retained_layout(block)
  end,
  compact = function(block)
    local meta = (block.output and block.output.metadata) or block.args or {}
    local mode = meta.edit_mode or "replace"
    local cell_type = meta.cell_type or ""
    local cell_label = cell_type ~= "" and (cell_type .. " cell") or "cell"
    local new_source = block.output and block.output.content_fields and block.output.content_fields.new_source
    local lines = (new_source and new_source.content_lines) or smelt.text.line_count(meta.new_source or "")
    if mode == "delete" then return "deleted " .. cell_label end
    local verb = ({ insert = "inserted", replace = "replaced" })[mode] or (mode .. "d")
    return verb .. " " .. cell_label .. ", " .. tostring(lines) .. " lines"
  end,
})

smelt.tools.register({
  name = "edit_notebook",
  description = "Edit a Jupyter notebook (.ipynb) cell. Supports replacing, inserting, and deleting cells. Identify cells by cell_id or cell_number (0-indexed).",
  override = true,
  execution_mode = "sequential",
  effect = "write",
  parameters = {
    type = "object",
    properties = {
      notebook_path = {
        type = "string",
        description = "The absolute path to the Jupyter notebook file",
      },
      cell_number = {
        type = "integer",
        description = "The 0-indexed cell number to edit. Used when cell_id is not provided.",
      },
      cell_id = {
        type = "string",
        description = "The ID of the cell to edit. Takes precedence over cell_number. When inserting, the new cell is placed after this cell (omit to insert at the beginning).",
      },
      new_source = {
        type = "string",
        description = "The new source content for the cell. Required for replace and insert.",
      },
      cell_type = {
        type = "string",
        enum = { "code", "markdown" },
        description = "The cell type. Required for insert, defaults to current type for replace.",
      },
      edit_mode = {
        type = "string",
        enum = { "replace", "insert", "delete" },
        description = "The edit operation. Defaults to replace.",
      },
    },
    required = { "notebook_path" },
  },
  summary = function(args, ctx)
    return smelt.tools.path_summary(args.notebook_path or "", ctx)
  end,
  preflight = function(args)
    local path = args.notebook_path or ""
    if path == "" then return nil end
    return smelt.fs.file_state.staleness_error(path, "notebook")
  end,
  paths_for_workspace = function(args)
    local p = args.notebook_path or ""
    return p ~= "" and { { path = p, kind = "file" } } or {}
  end,
  preview = function(args)
    local meta = smelt.notebook.preview_data(args)
    if not meta then
      return nil
    end
    return preview_layout(meta)
  end,
  execute = function(args)
    local path = args.notebook_path or ""
    if path == "" then
      return { content = "notebook_path is required", is_error = true }
    end
    local result, err = smelt.notebook.apply_edit_async(args)
    if not result then
      return { content = err or "notebook edit failed", is_error = true }
    end

    local metadata = result.metadata or {}
    local old_source = metadata.old_source or ""
    local new_source = metadata.new_source or ""
    metadata.old_source = nil
    metadata.new_source = nil
    return {
      content = result.message,
      metadata = metadata,
      display_content = {
        old_source = old_source,
        new_source = new_source,
      },
    }
  end,
})
