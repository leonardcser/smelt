-- Clear active search highlights.

local function clear_search()
  smelt.search.clear()
end

smelt.cmd.register("nohl", clear_search, { desc = "clear search highlights" })
smelt.cmd.register("nohlsearch", clear_search, { desc = "clear search highlights", hidden = true })
smelt.cmd.register("noh", clear_search, { desc = "clear search highlights", hidden = true })
