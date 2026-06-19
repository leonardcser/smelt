-- Built-in web_search tool. DuckDuckGo HTML search with 15-minute cache and rotated UA.

local function urlencode(s)
  return (
    s:gsub("([^A-Za-z0-9_.~-])", function(c)
      return string.format("%%%02X", string.byte(c))
    end)
  )
end

local function is_duckduckgo_challenge(status, body)
  if not body then return false end
  return (status == 202 and body:find("anomaly", 1, true) ~= nil)
    or body:find("anomaly-modal", 1, true) ~= nil
    or body:find("anomaly.js", 1, true) ~= nil
    or body:find("Unfortunately, bots use DuckDuckGo too.", 1, true) ~= nil
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
      return { content = "query cannot be empty", is_error = true }
    end

    local cache_key = "search:" .. query
    local cached = smelt.http.cache.read(cache_key)
    if cached then
      return cached
    end

    local encoded_query = urlencode(query)
    local url = "https://html.duckduckgo.com/html/?q=" .. encoded_query .. "&kl=us-en"
    local resp, err = smelt.http.get(url, {
      timeout_secs = 20,
      max_redirects = 10,
      headers = {
        ["User-Agent"] = smelt.http.random_user_agent(),
        ["Accept"] = "text/html",
        ["Accept-Language"] = "en-US,en;q=0.9",
        ["Referer"] = "https://html.duckduckgo.com/html/",
      },
    })
    if not resp then
      return { content = "search failed: " .. (err or "no response"), is_error = true }
    end
    local body = resp.body or ""
    if is_duckduckgo_challenge(resp.status, body) then
      return { content = "search failed: DuckDuckGo returned an anti-bot challenge", is_error = true }
    end
    if resp.status ~= 200 then
      return { content = "search failed: HTTP " .. resp.status, is_error = true }
    end

    local results = smelt.html.parse_ddg_results(body)
    if #results == 0 then
      return "no results found"
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
