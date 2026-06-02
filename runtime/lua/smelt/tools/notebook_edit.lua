-- Built-in edit_notebook tool. Replace/insert/delete a Jupyter cell with
-- staleness preflight and per-path flock.

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
  summary = function(args)
    return smelt.path.display(args.notebook_path or "")
  end,
  preflight = function(args)
    local path = args.notebook_path or ""
    if path == "" then
      return nil
    end
    return smelt.fs.file_state.staleness_error(path, "notebook")
  end,
  preview = function(args)
    local data = smelt.notebook.preview_data(args)
    if not data then return nil end
    local title = smelt.layout.text(data.title)
    local body
    if data.edit_mode == "insert" then
      body = smelt.layout.file_view({
        content = data.new_source,
        lang    = data.syntax_ext,
      })
    else
      body = smelt.layout.diff({
        old  = data.old_source,
        new  = data.new_source,
        lang = data.syntax_ext,
        path = data.path,
      })
    end
    return smelt.layout.vbox({ title, body })
  end,
  render = function(_, output, ctx)
    if output.is_error then
      return smelt.layout.tool_output(output, ctx)
    end
    local meta = output.metadata or {}
    if meta.edit_mode == "insert" then
      return smelt.layout.file_view({
        content = meta.new_source or "",
        path    = (meta.path or "") .. ".py",
      })
    end
    return smelt.layout.diff({
      old = meta.old_source or "",
      new = meta.new_source or "",
      path = meta.path or "",
    })
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
