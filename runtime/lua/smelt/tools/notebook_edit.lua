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

smelt.transcript.register_tool("edit_notebook", {
  cache_key = "smelt.tool-presentation.edit_notebook:v1",
  body = function(block)
    return preview_layout((block.output and block.output.metadata) or {})
  end,
  compact = function(block)
    local meta = (block.output and block.output.metadata) or block.args or {}
    local mode = meta.edit_mode or "replace"
    local cell_type = meta.cell_type or ""
    local cell_label = cell_type ~= "" and (cell_type .. " cell") or "cell"
    local lines = smelt.text.line_count(meta.new_source or "")
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
    if path == "" or smelt.fs.file_state.has(path) then
      return nil
    end
    return "read the notebook with read_file before editing"
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

    return {
      content = result.message,
      metadata = result.metadata,
    }
  end,
})
