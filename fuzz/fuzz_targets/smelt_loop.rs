#![no_main]

//! libFuzzer entry point. Decodes a `FuzzInput` from libFuzzer bytes via
//! `arbitrary` and runs it through `smelt_fuzz::run_scenario`, which
//! enforces every state, resource, and registry invariant. The body is
//! intentionally trivial - the scenario logic lives in `smelt_fuzz::lib`
//! so the same code path serves the fuzz target and the
//! `crash_to_scenario` converter.

use libfuzzer_sys::fuzz_target;
use smelt_fuzz::FuzzInput;

fuzz_target!(|input: FuzzInput| {
    smelt_fuzz::run_scenario(input);
});
