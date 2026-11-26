-- `smelt search`, and /search, which reads the same spec.

return {
  name = "search",
  description = "Search file contents and open matches in your editor",
  args = "QUERY",
  args_description = "Text to search for. Matches ignore case unless it has an uppercase letter",
  raw = true,
  flags = {
    { name = "git", short = "g", kind = "boolean", description = "Look in Git-tracked files only" },
    { name = "regex", short = "e", kind = "boolean", description = "Read QUERY as a regular expression" },
  },
}
