-- Declare `smelt files` and `smelt search`, the pickers that run without an
-- agent session. `smelt/plugins/file_finder.lua` opens them with
-- `smelt.cli.on_command`.

smelt.cli.register_command({
  name = "files",
  description = "Find files by name and open them in your editor; a leading `git` lists tracked files only",
  args = "QUERY",
})
smelt.cli.register_command({
  name = "search",
  description = "Search file contents and open matches in your editor; a leading `git` searches tracked files only",
  args = "QUERY",
})
