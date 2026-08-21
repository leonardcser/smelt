-- Built-in web_search tool. Configurable provider with 15-minute HTTP cache.

local USER_AGENT = "smelt-web-search/1"

local function urlencode(s)
  return (
    s:gsub("([^A-Za-z0-9_.~-])", function(c)
      return string.format("%%%02X", string.byte(c))
    end)
  )
end

local function setting(name, default)
  if not smelt.settings then return default end
  local value = smelt.settings[name]
  if value == nil or value == "" then return default end
  return value
end

local function is_duckduckgo_challenge(status, body)
  if not body then return false end
  return (status == 202 and body:find("anomaly", 1, true) ~= nil)
    or body:find("anomaly-modal", 1, true) ~= nil
    or body:find("anomaly.js", 1, true) ~= nil
    or body:find("Unfortunately, bots use DuckDuckGo too.", 1, true) ~= nil
end

local function format_results(results)
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

  return table.concat(lines, "\n")
end

local function search_duckduckgo(query)
  local encoded_query = urlencode(query)
  local url = "https://html.duckduckgo.com/html/?q=" .. encoded_query .. "&kl=us-en"
  local resp, err = smelt.http.get(url, {
    timeout_secs = 20,
    max_redirects = 10,
    max_response_bytes = 2 * 1024 * 1024,
    max_retries = 2,
    headers = {
      ["User-Agent"] = USER_AGENT,
      ["Accept"] = "text/html",
      ["Accept-Encoding"] = "identity",
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

  return format_results(smelt.html.parse_ddg_results(body))
end

local function brave_api_key()
  local env_name = setting("brave_search_api_key_env", "BRAVE_SEARCH_API_KEY")
  local key = smelt.os.getenv(env_name)
  if not key or key == "" then
    return nil, "search failed: Brave Search API key env var '" .. env_name .. "' is not set"
  end
  return key, nil
end

local function search_brave(query)
  local key, key_err = brave_api_key()
  if not key then
    return { content = key_err, is_error = true }
  end

  local url = "https://api.search.brave.com/res/v1/web/search?q=" .. urlencode(query)
  local resp, err = smelt.http.get(url, {
    timeout_secs = 20,
    max_redirects = 3,
    max_response_bytes = 2 * 1024 * 1024,
    max_retries = 2,
    headers = {
      ["User-Agent"] = USER_AGENT,
      ["Accept"] = "application/json",
      ["Accept-Encoding"] = "identity",
      ["X-Subscription-Token"] = key,
    },
  })
  if not resp then
    return { content = "search failed: " .. (err or "no response"), is_error = true, retryable = true }
  end
  if resp.status ~= 200 then
    local retryable = resp.status == 408 or resp.status == 429 or resp.status >= 500
    return { content = "search failed: HTTP " .. resp.status, is_error = true, retryable = retryable }
  end

  local decoded, decode_err = smelt.json.decode(resp.body or "")
  if not decoded then
    return {
      content = "search failed: invalid Brave response: " .. (decode_err or "invalid JSON"),
      is_error = true,
      retryable = true,
    }
  end

  local results = {}
  local web = type(decoded.web) == "table" and decoded.web or {}
  local web_results = type(web.results) == "table" and web.results or {}
  for _, r in ipairs(web_results) do
    if type(r) == "table" then
      local title = r.title or ""
      local link = r.url or ""
      if title ~= "" and link ~= "" then
        table.insert(results, {
          title = title,
          link = link,
          description = r.description or "",
        })
      end
      if #results >= 20 then break end
    end
  end

  return format_results(results)
end

local providers = {
  duckduckgo = search_duckduckgo,
  brave = search_brave,
}

smelt.tools.register({
  name = "web_search",
  description = "Search the web using the configured search provider. Returns a list of results with titles, URLs, and descriptions.",
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

    local configured_provider = setting("web_search_provider", "auto")
    local provider = configured_provider
    if provider == "auto" then
      provider = brave_api_key() and "brave" or "duckduckgo"
    end
    local search = providers[provider]
    if not search then
      return { content = "search failed: unknown web search provider '" .. provider .. "'", is_error = true }
    end

    local cache_key = "search:" .. provider .. ":" .. query
    local cached = smelt.http.cache.read(cache_key)
    if cached then
      return cached
    end

    local output = search(query)
    if type(output) == "table" and output.is_error and output.retryable
      and configured_provider == "auto" and provider == "brave"
    then
      local brave_error = output.content
      provider = "duckduckgo"
      cache_key = "search:" .. provider .. ":" .. query
      output = smelt.http.cache.read(cache_key) or search_duckduckgo(query)
      if type(output) == "table" and output.is_error then
        output.content = brave_error .. "; DuckDuckGo fallback also failed: " .. output.content
      end
    end
    if type(output) == "table" and output.is_error then
      return output
    end

    smelt.http.cache.write(cache_key, output)
    return output
  end,
})
