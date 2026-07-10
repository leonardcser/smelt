-- Optional LSP tool facade for agent code navigation. It adds semantic tool
-- names and LSP-aware guidance without changing the base tool descriptions.
-- Servers use the generic stdio Language Server Protocol backend.

local M = {}

local state = {
  servers = {},
  regs = {},
}

local COLLAPSED_TOOLS = {
  "outline",
  "find_symbol",
  "inspect_symbol",
  "inspect_symbol_at",
  "find_references",
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

local JSON_PREVIEW_LIMIT = 200
local JSON_OBJECT_KEY_LIMIT = 200
local JSON_DEPTH_LIMIT = 8

local function is_array(t)
  if type(t) ~= "table" then return false end
  local n = #t
  if n == 0 then return next(t) == nil end
  for k, _ in pairs(t) do
    if type(k) ~= "number" or k < 1 or k > n or k % 1 ~= 0 then return false end
  end
  return true
end

local function limited_json_value(value, depth)
  if type(value) ~= "table" then return value end
  if depth >= JSON_DEPTH_LIMIT then return "<truncated: maximum preview depth reached>" end

  if is_array(value) then
    local total = #value
    local keep = math.min(total, JSON_PREVIEW_LIMIT)
    local items = {}
    for i = 1, keep do
      items[i] = limited_json_value(value[i], depth + 1)
    end
    if keep < total then
      return {
        truncated = true,
        total = total,
        shown = keep,
        omitted = total - keep,
        items = items,
      }
    end
    return items
  end

  local out = {}
  local shown = 0
  local omitted = 0
  for k, v in pairs(value) do
    if shown < JSON_OBJECT_KEY_LIMIT then
      out[k] = limited_json_value(v, depth + 1)
      shown = shown + 1
    else
      omitted = omitted + 1
    end
  end
  if omitted > 0 then
    out._truncated = true
    out._omitted_keys = omitted
  end
  return out
end

local function format_json_result(result)
  local preview = limited_json_value(result, 0)
  return {
    content = smelt.json.encode(preview, { pretty = true }),
    metadata = { syntax = "json" },
  }
end

local function table_len(t)
  return type(t) == "table" and #t or 0
end

local function plural(n, word)
  if n == 1 then return "1 " .. word end
  return tostring(n or 0) .. " " .. word .. "s"
end

local function count_text(shown, word, omitted)
  local text = plural(shown, word)
  if omitted and omitted > 0 then text = text .. " shown (" .. omitted .. " omitted)" end
  return text
end

local function with_display_count(content, value, unit)
  return {
    content = content,
    metadata = { display_count = { value = value or 0, unit = unit } },
  }
end

local function format_kind(kind)
  if kind == "function" then return "fn" end
  return kind or "symbol"
end

local function format_symbol_label(label)
  if type(label) ~= "string" then return label end
  return label:gsub("^function%s+", "fn ")
end

local function decode_uri_path(path)
  path = path:gsub("%%(%x%x)", function(hex)
    return string.char(tonumber(hex, 16))
  end)
  if path:match("^/[A-Za-z]:/") then path = path:sub(2) end
  return path
end

local function display_path(path)
  if type(path) == "string" and smelt.path and smelt.path.display then
    return smelt.path.display(path)
  end
  return path
end

local function file_uri_to_path(uri)
  local path = uri:match("^file://localhost(/.*)$") or uri:match("^file://(/.*)$")
  if not path then return uri end
  return display_path(decode_uri_path(path))
end

local function loc_line(loc)
  if type(loc) ~= "table" then return nil end
  local path = loc.file_path or loc.uri or loc.targetUri or "<unknown>"
  if type(path) == "string" and path:match("^file://") then path = file_uri_to_path(path) end
  if type(path) == "string" then path = display_path(path) end
  local line = loc.line
  local column = loc.column
  if (not line or not column) and loc.range and loc.range.start then
    line = (loc.range.start.line or 0) + 1
    column = (loc.range.start.character or 0) + 1
  end
  if (not line or not column) and loc.targetSelectionRange and loc.targetSelectionRange.start then
    line = (loc.targetSelectionRange.start.line or 0) + 1
    column = (loc.targetSelectionRange.start.character or 0) + 1
  end
  local text = string.format("%s:%s:%s", path, line or "?", column or "?")
  if loc.preview and loc.preview ~= "" then text = text .. " - " .. loc.preview end
  return text
end

local function append_locations(lines, title, locations)
  if type(locations) == "table" and locations.error then
    table.insert(lines, title .. ": " .. tostring(locations.error))
    return
  end
  local count = table_len(locations)
  if count == 0 then return end
  table.insert(lines, title .. ": " .. plural(count, "location"))
  for _, loc in ipairs(locations or {}) do
    table.insert(lines, "- " .. (loc_line(loc) or tostring(loc)))
  end
end

local function outline_range(symbol)
  if not symbol.line then return "" end
  local start_text = string.format("%s:%s", symbol.line, symbol.column or 1)
  if symbol.end_line then
    return string.format(" %s-%s:%s", start_text, symbol.end_line, symbol.end_column or 1)
  end
  return " " .. start_text
end

local function format_outline_tree(lines, symbols, indent)
  for _, symbol in ipairs(symbols or {}) do
    table.insert(lines, string.rep("  ", indent) .. string.format("- %s %s%s", format_kind(symbol.kind), symbol.name or "<anonymous>", outline_range(symbol)))
    format_outline_tree(lines, symbol.children, indent + 1)
  end
end

local function format_outline(result)
  if result == nil then return "0 symbols" end
  if type(result) ~= "table" or type(result.symbols) ~= "table" then return nil end
  local shown = result.shown or table_len(result.symbols)
  local lines = { count_text(shown, "symbol", result.omitted) }
  format_outline_tree(lines, result.symbols, 0)
  return table.concat(lines, "\n")
end

local function append_messages(lines, title, items)
  if table_len(items) == 0 then return end
  for _, item in ipairs(items or {}) do
    local where = item.server and (" (" .. tostring(item.server) .. ")") or ""
    local message = item.message or item.error or item.status or "unknown"
    table.insert(lines, title .. ": " .. tostring(message) .. where)
  end
end

local function format_symbol_results(result)
  local shown = result.shown or table_len(result.symbols)
  local lines = { count_text(shown, "symbol", result.omitted) }
  for _, symbol in ipairs(result.symbols or {}) do
    local path = symbol.file_path or "<unknown>"
    local at = string.format("%s:%s:%s", path, symbol.line or "?", symbol.column or "?")
    local suffix = symbol.server and (" [" .. symbol.server .. "]") or ""
    local rank = symbol.rank and (" [" .. symbol.rank .. "]") or ""
    table.insert(lines, string.format("- %s %s - %s%s%s", format_kind(symbol.kind), symbol.name or "<anonymous>", at, suffix, rank))
    if symbol.detail then table.insert(lines, "  " .. symbol.detail) end
    if symbol.container_name then table.insert(lines, "  in " .. symbol.container_name) end
  end
  append_messages(lines, "errors", result.errors)
  return table.concat(lines, "\n")
end

local function format_references(result)
  if type(result) ~= "table" or not result.locations then return nil end
  local shown = result.shown or table_len(result.locations)
  local lines = { count_text(shown, "reference", result.omitted) }
  for _, loc in ipairs(result.locations or {}) do
    table.insert(lines, "- " .. (loc_line(loc) or tostring(loc)))
  end
  return table.concat(lines, "\n")
end

local function format_locations(title, result)
  if type(result) ~= "table" or not result.locations then return nil end
  local shown = result.shown or table_len(result.locations)
  local lines = { count_text(shown, title, result.omitted) }
  for _, loc in ipairs(result.locations or {}) do
    table.insert(lines, "- " .. (loc_line(loc) or tostring(loc)))
  end
  return table.concat(lines, "\n")
end

local function format_outline_context(context)
  local parts = {}
  for _, symbol in ipairs(context or {}) do
    table.insert(parts, format_kind(symbol.kind) .. " " .. (symbol.name or "<anonymous>"))
  end
  return table.concat(parts, " > ")
end

local function format_inspect(result)
  local lines = {}
  if type(result.enclosing_symbol) == "table" then
    table.insert(lines, "enclosing: " .. format_kind(result.enclosing_symbol.kind) .. " " .. (result.enclosing_symbol.name or "<anonymous>"))
  elseif result.enclosing_symbol then
    table.insert(lines, "enclosing: " .. format_symbol_label(result.enclosing_symbol))
  end
  local context = format_outline_context(result.outline_context)
  if context ~= "" then table.insert(lines, "outline: " .. context) end
  if type(result.hover) == "string" and result.hover ~= "" then
    table.insert(lines, "hover:\n" .. result.hover)
  elseif type(result.hover) == "table" and result.hover.error then
    table.insert(lines, "hover: " .. tostring(result.hover.error))
  end
  append_locations(lines, "definitions", result.definitions)
  append_locations(lines, "type definitions", result.type_definitions)
  append_locations(lines, "implementations", result.implementations)
  local refs = format_references(result.references)
  if refs then table.insert(lines, refs) end
  if #lines == 0 then return "no symbol details" end
  return table.concat(lines, "\n")
end

local function severity_name(value)
  return ({ [1] = "error", [2] = "warning", [3] = "info", [4] = "hint" })[value] or tostring(value or "diagnostic")
end

local function append_diagnostic(lines, diag, path)
  local start = diag.range and diag.range.start or {}
  local line = (start.line or 0) + 1
  local col = (start.character or 0) + 1
  local source = diag.source and (" [" .. diag.source .. "]") or ""
  if type(path) == "string" and path:match("^file://") then path = file_uri_to_path(path) end
  if type(path) == "string" then path = display_path(path) end
  table.insert(lines, string.format("- %s%s %s:%s:%s - %s", severity_name(diag.severity), source, path or "<unknown>", line, col, diag.message or ""))
end

local function diagnostic_count(result)
  if is_array(result) then return #result end
  local count = 0
  for _, diagnostics in pairs(result or {}) do count = count + table_len(diagnostics) end
  return count
end

local function format_diagnostics(result, args)
  local count = diagnostic_count(result)
  if count == 0 then return "no diagnostics" end
  local lines = { plural(count, "diagnostic") }
  if is_array(result) then
    for _, diag in ipairs(result) do append_diagnostic(lines, diag, args and args.file_path) end
  else
    for uri, diagnostics in pairs(result or {}) do
      for _, diag in ipairs(diagnostics or {}) do append_diagnostic(lines, diag, uri) end
    end
  end
  return table.concat(lines, "\n")
end

local function format_rename(result)
  local summary = result.summary or {}
  local action = result.applied and "applied" or "preview"
  local lines = { string.format("%s across %s", action, plural(summary.file_count or table_len(summary.files), "file")) }
  for _, file in ipairs(summary.files or {}) do
    local path = file.uri or file.file_path or "<unknown>"
    if type(path) == "string" and path:match("^file://") then path = file_uri_to_path(path) end
    if type(path) == "string" then path = display_path(path) end
    table.insert(lines, string.format("- %s (%s)", path, plural(file.edits or 0, "edit")))
  end
  return table.concat(lines, "\n")
end

local function format_structured_result(name, args, result)
  if name == "outline" then
    local formatted = format_outline(result)
    if formatted then
      local shown = type(result) == "table" and (result.shown or table_len(result.symbols)) or 0
      return with_display_count(formatted, shown, "symbol")
    end
  end
  if name == "find_symbol" and type(result) == "table" then
    return with_display_count(format_symbol_results(result), result.shown or table_len(result.symbols), "symbol")
  end
  if (name == "inspect_symbol" or name == "inspect_symbol_at") and type(result) == "table" then
    return with_display_count(format_inspect(result), 1, "symbol")
  end
  if name == "find_references" and not (args and args.raw) then
    local formatted = format_references(result)
    if formatted then return with_display_count(formatted, result.shown or table_len(result.locations), "reference") end
  end
  if name == "find_definition" then
    local formatted = format_locations("definition", result)
    if formatted then return with_display_count(formatted, result.shown or table_len(result.locations), "definition") end
  end
  if name == "diagnostics" then
    return with_display_count(format_diagnostics(result, args), diagnostic_count(result), "diagnostic")
  end
  if (name == "preview_rename" or name == "rename_symbol") and type(result) == "table" then return { content = format_rename(result) } end
  return format_json_result(result)
end

local function backend_operation(tool_name)
  return ({
    language_server_status = "status",
    outline = "outline",
    find_symbol = "workspace_symbols",
    inspect_symbol_at = "inspect_symbol_at",
    inspect_symbol = "inspect_symbol",
    find_definition = "definition",
    find_references = "references",
    diagnostics = "diagnostics",
    preview_rename = "rename_preview",
    rename_symbol = "rename",
  })[tool_name]
end

local function call_backend(name, args)
  args = args or {}
  local operation = backend_operation(name)
  if not operation or not smelt.lsp or not smelt.lsp.__call then
    return { content = "Language server backend is unavailable.", is_error = true }
  end
  local payload = smelt.task.external(function(id)
    smelt.lsp.__call(id, operation, args)
  end)
  if payload.err then return { content = payload.err, is_error = true } end
  local result = payload.result
  if type(result) == "string" then return { content = result } end
  return format_structured_result(name, args, result)
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

local function quoted(value)
  return string.format("%q", tostring(value))
end

local function position_summary(args, ctx)
  local path = smelt.tools.path_summary(args.file_path or "", ctx)
  if path == "" then return "" end
  if args.line then path = path .. ":" .. tostring(args.line) end
  if args.column then path = path .. ":" .. tostring(args.column) end
  return path
end

local function outline_summary(args, ctx)
  local parts = {}
  local path = smelt.tools.path_summary(args.file_path or "", ctx)
  if path ~= "" then parts[#parts + 1] = path end
  if args.symbol and args.symbol ~= "" then
    parts[#parts + 1] = "symbol " .. quoted(args.symbol)
  elseif args.kind and args.kind ~= "" and args.name_contains and args.name_contains ~= "" then
    parts[#parts + 1] = tostring(args.kind) .. " containing " .. quoted(args.name_contains)
  else
    if args.kind and args.kind ~= "" then parts[#parts + 1] = tostring(args.kind) end
    if args.name_contains and args.name_contains ~= "" then parts[#parts + 1] = "containing " .. quoted(args.name_contains) end
  end
  if args.max_depth ~= nil then parts[#parts + 1] = "depth " .. tostring(args.max_depth) end
  return table.concat(parts, ", ")
end

local function tool_summary(name, args, ctx)
  args = args or {}
  if name == "outline" then return outline_summary(args, ctx) end
  if name == "find_symbol" or name == "inspect_symbol" then
    local parts = { tostring(args.query or "") }
    if args.kind and args.kind ~= "" then parts[#parts + 1] = tostring(args.kind) end
    if args.path_glob and args.path_glob ~= "" then parts[#parts + 1] = tostring(args.path_glob) end
    return table.concat(parts, ", ")
  end
  local summary = position_summary(args, ctx)
  if summary ~= "" and args.new_name and args.new_name ~= "" then
    summary = summary .. " to " .. quoted(args.new_name)
  end
  if summary ~= "" then return summary end
  return args.query or ""
end

local function configure_transcript_defaults()
  if smelt.settings and type(smelt.settings.transcript) == "table" then
    local transcript = smelt.settings.transcript
    if type(transcript.view) ~= "table" then transcript.view = {} end
    if type(transcript.view.tools) ~= "table" then transcript.view.tools = {} end
    for _, name in ipairs(COLLAPSED_TOOLS) do
      if transcript.view.tools[name] == nil then transcript.view.tools[name] = "collapsed" end
    end
  end

  local defaults = smelt.transcript and smelt.transcript.defaults
  if not defaults or type(defaults.__tool_collapsed_details) ~= "table" then return end
  for _, name in ipairs(COLLAPSED_TOOLS) do
    defaults.__tool_collapsed_details[name] = function(block)
      local metadata = block.output and block.output.metadata
      if type(metadata) ~= "table" or type(metadata.display_count) ~= "table" then return nil end
      return defaults.display_count_text(block)
    end
  end
end

local function register_tool(name, description, properties, required)
  local parameters = {
    type = "object",
    properties = properties,
  }
  if required and #required > 0 then parameters.required = required end
  add_reg(smelt.tools.register(smelt.tools._with_watchdog({
    name = name,
    description = description,
    permission_defaults = { normal = "allow", plan = "allow", apply = name == "rename_symbol" and "ask" or "allow" },
    effect = name == "rename_symbol" and "write" or "read",
    parameters = parameters,
    summary = function(args, ctx)
      return tool_summary(name, args, ctx)
    end,
    paths_for_workspace = function(args)
      local p = (args and args.file_path) or ""
      return p ~= "" and { { path = p, kind = "file" } } or {}
    end,
    execute = function(args)
      return call_backend(name, args)
    end,
  }, { default_ms = 125000, max_ms = 240000, grace_ms = 10000 })))
end

local function patch_existing_tools()
  add_reg(smelt.tools.patch("glob", {
    description = "Fast file pattern matching by path. Useful for locating files by name or glob before more specific inspection.",
  }))
  add_reg(smelt.tools.patch("grep", {
    description = "Regex search over file contents. Useful for text, comments, strings, docs, config, generated text, and non-symbol matches.",
  }))
  add_reg(smelt.tools.patch("edit_file", {
    description = "Perform exact string replacements in files after the target file content is known.",
  }))
end

local function add_guidance()
  add_reg(smelt.agent.add_system_prompt([[# Semantic code tools
Semantic code tools can locate symbols, file outlines, definitions, references, callers, diagnostics, and renames. They describe code structure and symbol relationships when a matching language server is configured. Results may be unavailable, stale, or incomplete when servers are still starting or language support is partial.]]))
end

function M.setup(opts)
  opts = opts or {}
  clear_regs()
  state.servers = opts.servers or {}
  if smelt.lsp and smelt.lsp.configure then
    smelt.lsp.configure({ servers = state.servers })
  end

  patch_existing_tools()
  add_guidance()
  configure_transcript_defaults()

  register_tool("language_server_status", "Debug language server configuration, startup state, project roots, and stderr.", {
    file_path = path_param("Optional source file used to infer the language server."),
  }, {})

  register_tool("outline", "Return a compact semantic outline for a source file, with optional symbol, kind, name, and depth filters and source ranges when available.", {
    file_path = path_param(),
    max_symbols = { type = "integer", description = "Maximum outline symbols to return. Defaults to 200 and is capped by the backend." },
    symbol = { type = "string", description = "Only include this exact symbol name and ancestors/children needed for context." },
    kind = { type = "string", description = "Optional symbol kind filter, such as function, method, class, struct, trait, interface, module, or enum." },
    name_contains = { type = "string", description = "Only include symbols whose names contain this text, case-insensitive." },
    max_depth = { type = "integer", description = "Maximum nesting depth for unfiltered outlines, with 0 showing only top-level symbols. Filtered outlines still search nested symbols and return matching context." },
  }, { "file_path" })

  register_tool("find_symbol", "Find workspace symbols by semantic name search. Results are ranked, optionally filtered by kind, path, and exact match, and include limit and truncation metadata.", {
    query = { type = "string", description = "Symbol name or fuzzy query to send to the language server." },
    kind = { type = "string", description = "Optional symbol kind filter, such as function, method, class, struct, trait, interface, module, enum, variable, or constant." },
    path_glob = { type = "string", description = "Optional glob filter applied to returned file paths, such as crates/core/**/*.rs." },
    limit = { type = "integer", description = "Maximum symbols to return. Defaults to 20 and is capped by the backend." },
    exact = { type = "boolean", description = "Only return exact symbol-name matches after case-sensitive ranking. Defaults to false." },
  }, { "query" })

  register_tool("inspect_symbol_at", "Inspect a symbol at a source position using semantic data: hover info, definitions, type definitions, implementations, outline context, and summarized references.", position_params({
    depth = { type = "integer", description = "Expansion depth. 0 returns identity and direct links; 1 also includes summarized references. Defaults to 1." },
  }), { "file_path", "line", "column" })

  register_tool("inspect_symbol", "Inspect a symbol resolved from a workspace query using semantic data: hover info, definitions, type definitions, implementations, outline context, and summarized references.", {
    query = { type = "string", description = "Symbol query to resolve before inspection." },
    kind = { type = "string", description = "Optional symbol kind filter used with query." },
    path_glob = { type = "string", description = "Optional path glob used with query." },
    exact = { type = "boolean", description = "Require an exact symbol-name match. Defaults to true." },
    depth = { type = "integer", description = "Expansion depth. 0 returns identity and direct links; 1 also includes summarized references. Defaults to 1." },
  }, { "query" })

  register_tool("find_definition", "Find the definition for the symbol at a source position.", position_params({}), { "file_path", "line", "column" })

  register_tool("find_references", "Find semantic references to a symbol at a source position. Returns normalized locations with snippets by default, or raw language-server locations with raw=true. Both modes honor limit and report truncation metadata.", position_params({
    include_declaration = { type = "boolean", description = "Include the defining declaration when supported. Defaults to false." },
    limit = { type = "integer", description = "Maximum locations to show. Defaults to 50 and is capped by the backend." },
    raw = { type = "boolean", description = "Return raw language-server locations instead of normalized locations. Defaults to false." },
  }), { "file_path", "line", "column" })

  register_tool("diagnostics", "Return language server diagnostics for a file or workspace.", {
    file_path = path_param("Optional source file. Omit for workspace diagnostics when supported."),
  }, {})

  register_tool("preview_rename", "Preview a semantic code-symbol rename at a source position without applying edits.", position_params({
    new_name = { type = "string", description = "New symbol name." },
  }), { "file_path", "line", "column", "new_name" })

  register_tool("rename_symbol", "Apply a semantic code-symbol rename at a source position.", position_params({
    new_name = { type = "string", description = "New symbol name." },
  }), { "file_path", "line", "column", "new_name" })

  return smelt.reg.compose(table.unpack(state.regs))
end

M.setup()
return M
