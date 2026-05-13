//! Dev tooling. Invoke as `cargo xtask <command>` (see `.cargo/config.toml`).

mod gen_lua_docs;
mod synth;

fn main() {
    let mut args = std::env::args().skip(1);
    let cmd = args.next();
    match cmd.as_deref() {
        Some("gen-lua-docs") => gen_lua_docs::run(),
        Some("synth") => synth::run(),
        Some(other) => {
            eprintln!("xtask: unknown command `{other}`");
            print_usage();
            std::process::exit(2);
        }
        None => {
            print_usage();
            std::process::exit(2);
        }
    }
}

fn print_usage() {
    eprintln!("usage: cargo xtask <command>");
    eprintln!();
    eprintln!("commands:");
    eprintln!("  gen-lua-docs   regenerate Lua API stubs + reference docs");
    eprintln!("  synth          generate a synthetic session for perf testing");
}
