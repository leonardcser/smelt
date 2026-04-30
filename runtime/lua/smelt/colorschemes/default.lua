-- Default smelt colorscheme.
--
-- Applies the ember accent (ANSI 208). Other roles populate from the
-- accent via Rust-side `populate_ui_theme`. Load with
-- `require("smelt.colorschemes.default")` from `init.lua` to re-apply
-- (the same value is also the runtime default).

smelt.theme.set("accent", { ansi = 208 })

return {}
