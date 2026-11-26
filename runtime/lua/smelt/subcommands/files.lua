-- `smelt files`, and /files, which reads the same spec.

return {
  name = "files",
  description = "Find files by name and open them in your editor",
  args = "QUERY",
  args_description = "Name to start the search with",
  raw = true,
  flags = {
    { name = "git", short = "g", kind = "boolean", description = "Look in Git-tracked files only" },
  },
}
