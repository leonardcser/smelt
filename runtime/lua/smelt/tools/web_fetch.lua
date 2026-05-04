-- Built-in web_fetch tool — fetch a URL and extract content via an
-- isolated LLM call. Composes `smelt.http.{get,cache,random_user_agent}`,
-- `smelt.html.{title,links,to_text,to_markdown}`,
-- `smelt.image.data_url_from_bytes`, and `smelt.engine.ask` for the
-- extraction step. Mirrors the retired Rust `WebFetchTool`.

local MAX_RESPONSE_SIZE = 5 * 1024 * 1024
local DEFAULT_TIMEOUT = 30
local MAX_TIMEOUT = 120
local MAX_OUTPUT_LINES = 2000
local MAX_OUTPUT_BYTES = 50 * 1024

local IMAGE_MIMES = {
  "image/png",
  "image/jpeg",
  "image/gif",
  "image/webp",
  "image/bmp",
  "image/tiff",
}

local function url_host(url)
  local _, host = url:match("^([Hh][Tt][Tt][Pp][Ss]?)://([^/?#]+)")
  if host then host = host:lower() end
  return host
end

local function domain_pattern(url)
  local scheme, host = url:match("^([Hh][Tt][Tt][Pp][Ss]?)://([^/?#]+)")
  if not scheme or not host then return nil end
  return scheme:lower() .. "://" .. host:lower() .. "/*"
end

-- Match the retired Rust truncate_output: hard line cap, then char-
-- safe byte cap, then a one-line tail note.
local function truncate_output(text, max_lines, max_bytes)
  local lines = {}
  for line in (text .. "\n"):gmatch("([^\n]*)\n") do
    lines[#lines + 1] = line
  end
  local truncated = false
  if #lines > max_lines then
    while #lines > max_lines do table.remove(lines) end
    truncated = true
  end
  local result = table.concat(lines, "\n")
  if #result > max_bytes then
    local cut = max_bytes
    while cut > 0 do
      local b = result:byte(cut + 1)
      if not b or b < 0x80 or b >= 0xC0 then break end
      cut = cut - 1
    end
    result = result:sub(1, cut)
    truncated = true
  end
  if truncated then
    result = result .. "\n\n[output truncated]"
  end
  return result
end

local function header(headers, key)
  if not headers then return "" end
  return headers[key] or headers[key:lower()] or headers[key:upper()] or ""
end

local function fetch_raw(args)
  local url = args.url or ""
  local format = args.format
  if not format or format == "" then format = "markdown" end
  local timeout = math.min(tonumber(args.timeout) or DEFAULT_TIMEOUT, MAX_TIMEOUT)

  if not url:match("^[Hh][Tt][Tt][Pp][Ss]?://") then
    return { content = "URL must start with http:// or https://", is_error = true }
  end

  local req_host = url_host(url)
  if not req_host then
    return { content = "Invalid URL: " .. url, is_error = true }
  end

  local cache_key = "fetch:" .. url .. ":" .. format
  local cached = smelt.http.cache.get(cache_key)
  if cached then return cached end

  local function do_fetch(ua)
    return smelt.http.get(url, {
      timeout_secs = timeout,
      max_redirects = 10,
      headers = {
        ["User-Agent"] = ua,
        ["Accept"] = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        ["Accept-Language"] = "en-US,en;q=0.9",
      },
    })
  end

  local resp, err = do_fetch(smelt.http.random_user_agent())
  if err then
    return { content = "Fetch failed: " .. err, is_error = true }
  end
  if resp.status == 403 and header(resp.headers, "cf-mitigated"):lower() == "challenge" then
    resp, err = do_fetch("smelt")
    if err then
      return { content = "Fetch failed: " .. err, is_error = true }
    end
  end

  local final_host = url_host(resp.final_url or url)
  if final_host ~= req_host then
    return {
      content = string.format(
        "Redirect crossed domains: requested %s but landed on %s. "
          .. "Re-run with the final URL if intended.",
        req_host or "?",
        final_host or "?"
      ),
      is_error = true,
    }
  end

  if resp.status < 200 or resp.status >= 300 then
    return { content = "HTTP " .. resp.status, is_error = true }
  end

  local content_type = header(resp.headers, "content-type"):lower()
  for _, mime in ipairs(IMAGE_MIMES) do
    if content_type:find(mime, 1, true) then
      local primary = content_type:match("^([^;]+)") or mime
      primary = primary:gsub("%s+", "")
      local body = resp.body
      if #body > MAX_RESPONSE_SIZE then body = body:sub(1, MAX_RESPONSE_SIZE) end
      local data_url = smelt.image.data_url_from_bytes(body, primary)
      return "![image](" .. data_url .. ")"
    end
  end

  local body = resp.body
  local was_truncated = false
  if #body > MAX_RESPONSE_SIZE then
    body = body:sub(1, MAX_RESPONSE_SIZE)
    was_truncated = true
  end

  local is_html = content_type:find("text/html", 1, true)
    or content_type:find("xhtml", 1, true)

  local title, links, content
  if is_html then
    if format == "html" then
      content = body
      title = smelt.html.title(body)
      links = smelt.html.links(body, url) or {}
    else
      local md = smelt.html.to_markdown(body, url)
      title = md.title
      links = md.links or {}
      content = (format == "text") and smelt.html.to_text(body) or md.content
    end
  else
    title = nil
    links = {}
    content = body
  end

  local parts = {}
  if title and title ~= "" then
    parts[#parts + 1] = "# " .. title .. "\n\n"
  end
  parts[#parts + 1] = content
  if #links > 0 then
    parts[#parts + 1] = "\n\n## Links\n\n"
    for _, link in ipairs(links) do
      parts[#parts + 1] = "- " .. link .. "\n"
    end
  end

  local output = truncate_output(table.concat(parts), MAX_OUTPUT_LINES, MAX_OUTPUT_BYTES)
  if was_truncated then
    output = output .. "\n\n[Response truncated — original response exceeded 5 MB]"
  end
  smelt.http.cache.put(cache_key, output)
  return output
end

-- Synchronous wrapper around `smelt.engine.ask`: spawns the auxiliary
-- request, parks the coroutine, returns the response text once the
-- engine emits `EngineAskResponse`.
local function ask_extract(content, prompt)
  local id = smelt.task.alloc()
  smelt.engine.ask({
    system = "Answer the user's question based solely on the provided "
      .. "web page content. Be concise and direct.",
    question = "<content>\n" .. content .. "</content>\n\n" .. prompt,
    task = "btw",
    on_response = function(resp) smelt.task.resume(id, resp) end,
  })
  return smelt.task.wait(id)
end

smelt.tools.register({
  name = "web_fetch",
  description = "Fetch a URL and extract relevant content using the given prompt. "
    .. "The page is fetched, converted to markdown, then an isolated LLM call "
    .. "extracts only what the prompt asks for.",
  override = true,
  parameters = {
    type = "object",
    properties = {
      url = {
        type = "string",
        description = "The URL to fetch (must start with http:// or https://)",
      },
      prompt = {
        type = "string",
        description = "What to extract or answer from the page content",
      },
      format = {
        type = "string",
        enum = { "markdown", "text", "html" },
        description = "Output format. Default: markdown",
      },
      timeout = {
        type = "integer",
        description = "Timeout in seconds (max 120). Default: 30",
      },
    },
    required = { "url", "prompt" },
  },
  needs_confirm = function(args) return args.url or "" end,
  approval_patterns = function(args)
    local pat = domain_pattern(args.url or "")
    if pat then return { pat } end
    return {}
  end,
  render = function(args, output, width, ctx)
    ctx:text(output.content, output.is_error)
  end,
  execute = function(args)
    local raw = fetch_raw(args)
    if type(raw) == "table" and raw.is_error then return raw end
    local content = type(raw) == "table" and raw.content or raw
    local prompt = args.prompt or ""
    local extracted = ask_extract(content, prompt)
    if extracted and extracted ~= "" and not extracted:lower():match("^error:") then
      return extracted
    end
    return raw
  end,
})
