-- /copy-transcript — copy the full conversation to the system clipboard.
-- Exercises the smelt.api.transcript.text() and smelt.clipboard() APIs.

smelt.api.cmd.register("copy-transcript", function()
    local text = smelt.api.transcript.text()
    if #text == 0 then
        smelt.notify("nothing to copy")
        return
    end
    smelt.clipboard(text)
    smelt.notify("transcript copied (" .. #text .. " chars)")
end)
