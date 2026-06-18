-- Optional LSP tool facade for agent code navigation. It adds semantic tool
-- names and LSP-aware guidance without changing the base tool descriptions.
-- Servers use the generic stdio Language Server Protocol backend.

local M = {}

local state = {
  start = "background",
  servers = {},
  regs = {},
}

local function add_reg(reg)
  if reg then table.insert(state.regs, reg) end
end

local function clear_regs()
  for _, reg in ipairs(state.regs) do
    if reg and reg.remove then reg:remove() end
  end
  state.regs = {}
end

local function backend_name(tool_name)
  return ({
    lsp_status = "__status",
    lsp_document_symbols = "__document_symbols",
    lsp_definition = "__definition",
    lsp_references = "__references",
    lsp_diagnostics = "__diagnostics",
    lsp_rename_preview = "__rename_preview",
    lsp_rename = "__rename",
  })[tool_name]
end

local function call_backend(name, args)
  args = args or {}
  local private = backend_name(name)
  if not private or not smelt.lsp or not smelt.lsp[private] then
    return { content = "LSP backend is unavailable.", is_error = true }
  end
  local payload = smelt.task.external(function(id)
    smelt.lsp[private](id, args)
  end)
  if payload.err then return { content = payload.err, is_error = true } end
  local result = payload.result
  if type(result) == "string" then return { content = result } end
  return { content = smelt.json.encode(result or {}, { pretty = true }) }
end

local function path_param(desc)
  return { type = "string", description = desc or "Absolute path to the source file." }
end

local function position_params(properties)
  properties.file_path = path_param()
  properties.line = { type = "integer", description = "1-based line number of the symbol position." }
  properties.column = { type = "integer", description = "1-based character column of the symbol position." }
  return properties
end

local function register_tool(name, description, properties, required)
  add_reg(smelt.tools.register(smelt.tools._with_watchdog({
    name = name,
    description = description,
    permission_defaults = { normal = "allow", plan = "allow", apply = name == "lsp_rename" and "ask" or "allow" },
    effect = name == "lsp_rename" and "write" or "read",
    parameters = {
      type = "object",
      properties = properties,
      required = required or {},
    },
    summary = function(args)
      return smelt.path.display((args and args.file_path) or "")
    end,
    paths_for_workspace = function(args)
      local p = (args and args.file_path) or ""
      return p ~= "" and { { path = p, kind = "file" } } or {}
    end,
    execute = function(args)
      return call_backend(name, args)
    end,
  }, { default_ms = 30000, max_ms = 120000, grace_ms = 5000 })))
end

local function patch_existing_tools()
  add_reg(smelt.tools.patch("glob", {
    description = "Fast file pattern matching by path. Use to find likely files before reading or searching; prefer LSP tools for symbol trees, definitions, and references when available.",
  }))
  add_reg(smelt.tools.patch("grep", {
    description = "Regex search over file contents. Use for broad text discovery and for comments, docs, strings, tests, or config that semantic LSP tools may miss; prefer LSP references for symbol-specific code queries.",
  }))
  add_reg(smelt.tools.patch("edit_file", {
    description = "Perform exact string replacements in files. Use for deliberate textual edits after reading the file; prefer LSP rename for code-symbol renames, then grep for non-code references.",
  }))
end

local function add_guidance()
  add_reg(smelt.agent.add_system_prompt([[# LSP tools
- If LSP tools are available, prefer them for symbol trees, definitions, references, diagnostics, and code-symbol renames.
- LSP is semantic but not complete: after LSP rename or refactor, use grep when appropriate to catch comments, docs, strings, tests, config, and other non-code references.
- If an LSP tool reports no backend, stale diagnostics, or missing project setup, fall back to glob, grep, read_file, and manual edits.]]))
end

function M.setup(opts)
  opts = opts or {}
  clear_regs()
  state.start = opts.start or "background"
  state.servers = opts.servers or {}
  if smelt.lsp and smelt.lsp.configure then
    smelt.lsp.configure({ start = state.start, servers = state.servers })
  end

  patch_existing_tools()
  add_guidance()

  register_tool("lsp_status", "Report whether an optional LSP backend is configured and what language servers it can use. Use before relying on other LSP tools.", {
    file_path = path_param("Optional source file used to infer the language server."),
  }, {})

  register_tool("lsp_document_symbols", "Return the symbol tree for a source file. Use to explore a file structurally before reading large sections.", {
    file_path = path_param(),
  }, { "file_path" })

  register_tool("lsp_definition", "Find the definition for the symbol at a source position. Prefer over grep when following code symbols.", position_params({}), { "file_path", "line", "column" })

  register_tool("lsp_references", "Find semantic references to the symbol at a source position. Prefer over grep for code references, then grep for comments, strings, docs, tests, and config if needed.", position_params({
    include_declaration = { type = "boolean", description = "Include the defining declaration when supported. Defaults to false." },
  }), { "file_path", "line", "column" })

  register_tool("lsp_diagnostics", "Return LSP diagnostics for a file or workspace. Useful after edits, but validate with project tests or typecheck when available.", {
    file_path = path_param("Optional source file. Omit for workspace diagnostics when supported."),
  }, {})

  register_tool("lsp_rename_preview", "Preview a semantic code-symbol rename at a source position without applying edits. Review the affected files before applying.", position_params({
    new_name = { type = "string", description = "New symbol name." },
  }), { "file_path", "line", "column", "new_name" })

  register_tool("lsp_rename", "Apply a semantic code-symbol rename at a source position. Use for code symbols only; afterward grep for non-code references and run validation.", position_params({
    new_name = { type = "string", description = "New symbol name." },
  }), { "file_path", "line", "column", "new_name" })

  return smelt.reg.compose(table.unpack(state.regs))
end

M.setup()
return M
