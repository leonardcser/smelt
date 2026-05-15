//! Convert a libFuzzer crash artifact (raw bytes) into a JSON scenario.
//!
//! Usage:
//!
//! ```text
//! cargo run --bin crash_to_scenario -- <artifact-path> [out.json]
//! ```
//!
//! If `out.json` is omitted, the scenario is printed to stdout. The same
//! `Arbitrary` decoder libFuzzer uses inside `fuzz_target!` runs here, so
//! the resulting `Scenario` is exactly the input shape that produced the
//! original crash.

use arbitrary::{Arbitrary, Unstructured};
use smelt_fuzz::Scenario;
use std::path::Path;
use std::{env, fs, process};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 || args.len() > 3 {
        eprintln!("usage: crash_to_scenario <artifact-path> [out.json]");
        process::exit(2);
    }
    let artifact = Path::new(&args[1]);
    let bytes = match fs::read(artifact) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: read {}: {e}", artifact.display());
            process::exit(1);
        }
    };

    // libFuzzer feeds `arbitrary_take_rest` for the full byte budget; mirror
    // that here so artifacts decode identically to the fuzz iteration that
    // produced them.
    let u = Unstructured::new(&bytes);
    let scenario = match Scenario::arbitrary_take_rest(u) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: decode {}: {e}", artifact.display());
            process::exit(1);
        }
    };

    let json = match serde_json::to_string_pretty(&scenario) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: serialize: {e}");
            process::exit(1);
        }
    };

    if args.len() == 3 {
        let out = Path::new(&args[2]);
        if let Err(e) = fs::write(out, &json) {
            eprintln!("error: write {}: {e}", out.display());
            process::exit(1);
        }
        eprintln!("wrote {}", out.display());
    } else {
        println!("{json}");
    }
}
