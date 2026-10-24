#![no_main]

//! Deterministic simulation of the real engine task. A scripted provider and a
//! scripted frontend drive complete turns on paused virtual time, so cancels,
//! steering, retries, held tool requests and provider failures interleave
//! reproducibly. See `smelt_fuzz::engine_sim` for the oracles.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: smelt_fuzz::engine_sim::Input| {
    smelt_fuzz::engine_sim::run(input);
});
