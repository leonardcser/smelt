-- Built-in /docs command. Opens the smelt documentation website in the
-- default browser via Rust's centralized opener checks, or falls back to
-- copying the URL to the clipboard when auto-open is unavailable.

local DOCS_URL = "https://leonardcser.github.io/smelt/"

local function copy_fallback(reason)
  local ok = pcall(smelt.clipboard.write, DOCS_URL)
  if ok then
    smelt.notify.info(reason .. ": copied " .. DOCS_URL .. " to clipboard")
  else
    smelt.notify.error(reason .. ": open " .. DOCS_URL .. " manually")
  end
end

smelt.cmd.register("docs", function()
  local opened = smelt.os.open_url_if_available(DOCS_URL)
  if opened.opened then
    return
  end
  if opened.error then
    smelt.messages.append("error", "docs", "open_url failed: " .. tostring(opened.error))
    copy_fallback("can't open browser")
  else
    copy_fallback(opened.reason or "browser auto-open unavailable")
  end
end, { desc = "open the smelt documentation in your browser" })
