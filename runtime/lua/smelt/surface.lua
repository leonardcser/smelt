-- Where a tool such as /diff or /files shows: a modal overlay over the
-- session, or, launched as `smelt <name>`, the whole terminal. A standalone
-- tool is the only UI, so smelt quits once its overlay closes.

local M = {}
M.__index = M

--- A surface for one tool. `opts.standalone` fills the terminal; otherwise
--- the tool's overlay takes `opts.width` and `opts.height`, fractions of the
--- terminal.
function M.new(opts)
  return setmetatable({
    standalone = opts.standalone == true,
    width_share = opts.width,
    height_share = opts.height,
  }, M)
end

local function percent(share) return math.floor(share * 100 + 0.5) .. "%" end

--- Open the tool's modal overlay, sized for this surface. Takes the options
--- of `smelt.overlay.new` but `width`, `height` and `modal`.
function M:overlay(opts)
  opts.width = self.standalone and "100%" or percent(self.width_share)
  opts.height = self.standalone and "100%" or percent(self.height_share)
  opts.modal = true
  return smelt.overlay.new(opts)
end

--- Columns the tool's overlay gets.
function M:width()
  local width = smelt.terminal.size().width or 80
  return self.standalone and width or math.floor(width * self.width_share)
end

--- Report that the tool cannot run, before it closes. A standalone tool
--- makes smelt exit with code 1 and `reason` as the message; an overlay
--- shows `reason` as an error.
function M:fail(reason)
  if self.standalone then
    smelt.quit({ code = 1, message = reason })
  else
    smelt.notify.error(reason)
  end
end

return M
