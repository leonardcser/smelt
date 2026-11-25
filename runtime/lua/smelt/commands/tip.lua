smelt.cmd.register("tip", function()
  local tip = require("smelt.tips").random_tip()
  if not tip then
    smelt.notify.info("No tips available")
    return
  end
  local key = (tip.key or "") ~= "" and (tip.key .. ": ") or ""
  smelt.notify.info("Tip: " .. key .. tip.text)
end, { desc = "show a discovery tip" })
