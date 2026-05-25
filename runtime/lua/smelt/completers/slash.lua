-- `/cmd` completer — opens when the buffer starts with `/` and the cursor sits
-- inside the unwhitespaced command name. Enter dispatches the command; Tab
-- accepts the name + trailing space (which shows the dim arg placeholder
-- when the command declared `args`).

if not (smelt.prompt and smelt.prompt.completer) then return end

local function accent_ansi()
  local accent = smelt.theme.get("SmeltAccent")
  return accent and accent.fg and accent.fg.ansi
end

smelt.prompt.completer({
  prefix = "/",
  detect = function(text, cpos)
    if text == "" then return nil end
    if text:sub(1, 1) ~= "/" then return nil end
    if text:find("\n", 1, true) then return nil end
    if cpos < 1 then return nil end
    if cpos >= 2 and text:sub(2, cpos):find("%s") then return nil end
    return 0
  end,
  items = function()
    local ansi = accent_ansi()
    local out = {}
    for _, c in ipairs(smelt.cmd.list()) do
      if not c.hidden then
        out[#out + 1] = {
          label        = c.name,
          description  = c.desc,
          ansi_color   = ansi,
          label_color  = ansi,
        }
      end
    end
    return out
  end,
  query = function(text, _, cpos)
    return text:sub(2, cpos)
  end,
  accept = function(item, _, action)
    if action == "tab" then
      -- Insert "/cmd " (trailing space) so the arg completer can detect it.
      smelt.prompt.set_text("/" .. item.label .. " ")
      smelt.prompt.cursor(#item.label + 2)
      return
    end
    -- Enter: dispatch the command and clear the prompt. Lua-registered
    -- commands without args run inline; arg-taking commands open their own
    -- picker via `smelt.cmd.picker`.
    smelt.prompt.set_text("")
    smelt.cmd.run("/" .. item.label)
  end,
})
