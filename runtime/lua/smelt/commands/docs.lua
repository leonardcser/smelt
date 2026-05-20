-- Built-in /docs command. Opens the smelt documentation website in the
-- default browser via `smelt.os.open_url`, or falls back to copying the
-- URL to the clipboard and surfacing a toast when no GUI is reachable
-- (headless host, SSH without X forwarding, container, etc.).

local DOCS_URL = "https://leonardcser.github.io/smelt/"

-- On macOS and Windows the GUI is always reachable. On Linux/BSD we
-- gate on the same env vars `xdg-open` itself uses to find a display,
-- so SSH-with-X-forwarding still opens normally while a headless box
-- skips straight to the clipboard fallback.
local function has_display()
  local plat = smelt.os.platform()
  if plat == "macos" or plat == "windows" then return true end
  return smelt.os.getenv("DISPLAY") ~= nil
      or smelt.os.getenv("WAYLAND_DISPLAY") ~= nil
end

local function copy_fallback(reason)
  local ok = pcall(smelt.clipboard.write, DOCS_URL)
  if ok then
    smelt.notify(reason .. ": copied " .. DOCS_URL .. " to clipboard")
  else
    smelt.notify.error(reason .. ": open " .. DOCS_URL .. " manually")
  end
end

smelt.cmd.register("docs", function()
  if not has_display() then
    copy_fallback("no display")
    return
  end
  local ok, err = smelt.os.open_url(DOCS_URL)
  if not ok then
    smelt.messages.append("error", "docs", "open_url failed: " .. tostring(err))
    copy_fallback("can't open browser")
  end
end, { desc = "open the smelt documentation in your browser" })
