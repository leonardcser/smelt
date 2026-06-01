#![no_main]

//! libFuzzer entry for the Lua-API fuzz target. Drives generated
//! `smelt.*` calls (resource lifecycle, paint, state, commands,
//! keymaps, /reload) against a real `TestApp` to attack the
//! `crates/{core,tui}/src/lua/api/*` surface - currently the
//! lowest-covered area of the workspace.

use libfuzzer_sys::fuzz_target;
use smelt_fuzz::LuaScenario;

fuzz_target!(|scenario: LuaScenario| {
    smelt_fuzz::run_lua_scenario(scenario);
});
