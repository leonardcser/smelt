//! Dev tooling. Invoke as `cargo xtask <command>` (see `.cargo/config.toml`).

mod bench_transcript_layout;
mod fuzz;
mod gen_file_icons;
mod gen_lua_docs;
mod synth;

fn main() {
    let mut args = std::env::args().skip(1);
    let cmd = args.next();
    match cmd.as_deref() {
        Some("bench-transcript-layout") => bench_transcript_layout::run(args.collect()),
        Some("gen-file-icons") => gen_file_icons::run(args.collect()),
        Some("gen-lua-docs") => gen_lua_docs::run(),
        Some("synth") => synth::run(),
        Some("fuzz") => fuzz::run(args.collect()),
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
    eprintln!("usage: cargo xtask <command> [args]");
    eprintln!();
    eprintln!("commands:");
    eprintln!(
        "  bench-transcript-layout [--runs N] [--workloads CSV] run transcript layout benches"
    );
    eprintln!("  gen-file-icons [DEVICONS_DIR]          regenerate nvim-web-devicons registry");
    eprintln!("  gen-lua-docs                          regenerate Lua API stubs + reference docs");
    eprintln!(
        "  synth                                 generate a synthetic session for perf testing"
    );
    eprintln!("  fuzz run <target> [--fork N] [--cmin] fuzz a target until crash or Ctrl-C");
    eprintln!(
        "  fuzz triage <target> <artifact>       shrink a crash and print the minimal scenario"
    );
    eprintln!("  fuzz replay-regression                replay every committed regression seed");
    eprintln!("  fuzz coverage-snapshot [target...]    per-target source coverage snapshot");
}
