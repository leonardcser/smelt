-- Built-in edit_notebook tool — replace / insert / delete a Jupyter
-- notebook (`.ipynb`) cell. The intricate JSON cell munging lives
-- behind `smelt.notebook.apply_edit` (FFI into
-- `engine::tools::notebook::apply_edit`); this Lua wrapper supplies
-- the schema, the staleness preflight, the per-path flock, and the
-- typed metadata payload the confirm dialog renders for the preview
-- pane.

smelt.tools.register({
  name = "edit_notebook",
  description = "Edit a Jupyter notebook (.ipynb) cell. Supports replacing, inserting, and deleting cells. Identify cells by cell_id or cell_number (0-indexed).",
  override = true,
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
  needs_confirm = function(args)
    return smelt.path.display(args.notebook_path or "")
  end,
  preflight = function(args)
    local path = args.notebook_path or ""
    if path == "" then
      return nil
    end
    return smelt.fs.file_state.staleness_error(path, "notebook")
  end,
  execute = function(args)
    local path = args.notebook_path or ""
    if path == "" then
      return { content = "notebook_path is required", is_error = true }
    end
    if not smelt.fs.exists(path) then
      return {
        content = "file not found: " .. smelt.path.display(path),
        is_error = true,
      }
    end

    local lock, lock_err = smelt.fs.try_flock(path)
    if not lock then
      return { content = lock_err or "could not lock notebook", is_error = true }
    end

    local result, err = smelt.notebook.apply_edit(args)
    if not result then
      lock:release()
      return { content = err or "notebook edit failed", is_error = true }
    end

    lock:release()
    return {
      content = result.message,
      metadata = result.metadata,
    }
  end,
})
