---@meta

--- Terminal notification helpers and transient turn-notification state.
---@class smelt.notifications
local notifications = {}

---@class smelt.notifications.Status
---@field configured boolean Persistent turn-end notifications from `smelt.settings.notifications.turn_end`.
---@field once boolean One-shot notification for the next successful turn end.
---@field session boolean True when turn notifications are enabled for this app session.
---@field suppressed boolean True when turn notifications are disabled for this app session.
---@field override "on"|"off"|nil Session override, or nil when following config.
---@field enabled boolean True when the next successful turn end will notify.
---@field mode string Human-readable effective mode.

--- Send a terminal notification using the best supported terminal primitive.
---@type fun(message: string): boolean
notifications.send = nil

--- Notify at the next successful turn end, then clear the one-shot flag.
---@type fun()
notifications.enable_once = nil

--- Notify at every successful turn end until cleared or smelt exits.
---@type fun()
notifications.enable_session = nil

--- Suppress turn-end notifications until cleared or smelt exits.
---@type fun()
notifications.disable_session = nil

--- Clear the session override and follow `smelt.settings.notifications.turn_end` again.
---@type fun()
notifications.clear_session = nil

--- Clear one-shot and session override state.
---@type fun()
notifications.clear = nil

--- Return the current persistent and transient turn-end notification state.
---@type fun(): smelt.notifications.Status
notifications.status = nil

--- Return true when a turn_end payload should produce a notification, consuming one-shot state atomically.
---@type fun(payload: table?): boolean
notifications.consume_turn_end = nil

return notifications
