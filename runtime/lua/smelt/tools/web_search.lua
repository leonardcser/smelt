-- Built-in web_search tool. DuckDuckGo HTML search with 15-minute cache and rotated UA.

local function urlencode(s)
  return (
    s:gsub("([^A-Za-z0-9_.~-])", function(c)
      return string.format("%%%02X", string.byte(c))
    end)
  )
end

smelt.tools.register({
  name = "web_search",
  description = "Search the web using DuckDuckGo. Returns a list of results with titles, URLs, and descriptions.",
  override = true,
  effect = "network",
  parameters = {
    type = "object",
    properties = {
      query = {
        type = "string",
        description = "The search query",
      },
    },
    required = { "query" },
  },
  summary = function(args)
    return args.query or ""
  end,
  execute = function(args)
    local query = args.query or ""
    if query == "" then
      return { content = "Query cannot be empty", is_error = true }
    end

    local cache_key = "search:" .. query
    local cached = smelt.http.cache.read(cache_key)
    if cached then
      return cached
    end

    local body = "q=" .. urlencode(query) .. "&kl=us-en"
    local resp, err = smelt.http.post("https://html.duckduckgo.com/html/", body, {
      timeout_secs = 20,
      max_redirects = 10,
      headers = {
        ["User-Agent"] = smelt.http.random_user_agent(),
        ["Content-Type"] = "application/x-www-form-urlencoded",
        ["Accept"] = "text/html",
        ["Accept-Language"] = "en-US,en;q=0.9",
        ["Referer"] = "https://html.duckduckgo.com/html/",
        ["Origin"] = "https://html.duckduckgo.com",
      },
    })
    if not resp then
      return { content = "Search failed: " .. (err or "no response"), is_error = true }
    end
    if resp.status < 200 or resp.status >= 300 then
      return { content = "Search failed: HTTP " .. resp.status, is_error = true }
    end

    local results = smelt.html.parse_ddg_results(resp.body or "")
    if #results == 0 then
      return "No results found"
    end

    local lines = {}
    for i, r in ipairs(results) do
      table.insert(lines, i .. ". " .. r.title)
      table.insert(lines, "   " .. r.link)
      if r.description and r.description ~= "" then
        table.insert(lines, "   " .. r.description)
      end
      table.insert(lines, "")
    end
    while #lines > 0 and lines[#lines] == "" do
      table.remove(lines)
    end

    local output = table.concat(lines, "\n")
    smelt.http.cache.write(cache_key, output)
    return output
  end,
})
