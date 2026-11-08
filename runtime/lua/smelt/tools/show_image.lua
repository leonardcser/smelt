-- Built-in show_image tool. Displays an image file inline in the transcript: full-resolution
-- Kitty graphics when the terminal supports them, colored half-blocks otherwise. The tool
-- keeps a display snapshot, so the transcript shows what the user saw even after the file
-- changes.

local transcript_defaults = require("smelt.transcript.defaults")

smelt.transcript.register_tool("show_image", {
  cache_key = "smelt.tool-presentation.show_image:v1",
  body = function(block, ctx)
    local output = block.output
    if not output then return nil end
    local snapshot = output.content_fields and output.content_fields.image
    local meta = output.metadata or {}
    if transcript_defaults.child_failed(block) or not snapshot then
      return transcript_defaults.render_tool_output_tail(output, ctx)
    end
    return smelt.layout.image(snapshot.content_id, { label = meta.label, source = meta.path })
  end,
})

smelt.tools.register({
  name = "show_image",
  description = "Show an image file to the user inline in their terminal (png, jpg/jpeg, gif, webp, bmp). "
    .. "Use it to present a screenshot, chart, diagram, or any generated image. The user sees the image; "
    .. "you do not. To look at an image yourself, use read_file. "
    .. "Terminals without graphics support get a low-resolution color-block rendering.",
  effect = "read",
  headless = false,
  permission_defaults = { normal = "allow", plan = "allow", apply = "allow" },
  parameters = {
    type = "object",
    properties = {
      file_path = {
        type = "string",
        description = "The absolute path to the image file to show",
      },
    },
    required = { "file_path" },
  },
  summary = function(args, ctx)
    return smelt.tools.path_summary(args.file_path or "", ctx)
  end,
  paths_for_workspace = function(args)
    local p = args.file_path or ""
    return p ~= "" and { { path = p, kind = "file" } } or {}
  end,
  execute = function(args)
    local given = args.file_path or ""
    if given == "" then
      return { content = "missing required parameter: file_path", is_error = true }
    end
    local path = smelt.path.resolve(given)

    local snapshot, err = smelt.image.display_snapshot_async(path)
    if not snapshot then
      return { content = "cannot display " .. path .. ": " .. tostring(err), is_error = true }
    end

    return {
      content = "displayed " .. path .. " to the user",
      display_content = { image = snapshot.data },
      metadata = {
        path = path,
        label = string.format("[image: %s %dx%d]", smelt.image.label_from_path(path),
          snapshot.width, snapshot.height),
      },
    }
  end,
})
