-- Optional show_ansi tool: shows ANSI-colored text, from a file of program output or hand-written escapes.

-- Hand-written text spells ESC as `\e`, `\033` or `\x1b`.
local function decode(s)
  return (s:gsub("\\e", "\27"):gsub("\\033", "\27"):gsub("\\[xX]1[bB]", "\27"))
end

smelt.transcript.register_tool("show_ansi", {
  body = function(block)
    local fields = block.output and block.output.content_fields
    if not (fields and fields.ansi) then return nil end
    local text = smelt.layout.content(fields.ansi.content_id, { format = "text", ansi = true, hl_group = "Normal" })
    return smelt.layout.gutter(text, { text = "  " })
  end,
})

smelt.tools.register({
  name = "show_ansi",
  description = "Show ANSI-colored text to the user. Pass `path` for program output saved with color "
    .. "forced (e.g. `cmd --color=always > /tmp/out.ansi`), or `text` with escapes written as `\\e[...m`. "
    .. "Only colors and styles render; cursor movement is ignored.",
  access = "read",
  paths = function(args)
    return args.path and { args.path } or {}
  end,
  headless = false,
  parameters = {
    type = "object",
    properties = {
      path = { type = "string", description = "File to show" },
      text = { type = "string", description = "Text to show" },
    },
  },
  summary = function(args, ctx)
    return args.path and smelt.tools.path_summary(args.path, ctx) or ""
  end,
  execute = function(args)
    local text = args.text and decode(args.text)
    if args.path then
      local content, err = smelt.fs.read_async(smelt.path.resolve(args.path))
      if not content then return { content = tostring(err), is_error = true } end
      text = content
    end
    if not text or text == "" then
      return { content = "nothing to show: pass `path` or `text`", is_error = true }
    end
    return { content = "shown to the user", display_content = { ansi = text } }
  end,
})
