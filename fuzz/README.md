# smelt fuzz

## Setup

```bash
rustup toolchain install nightly
cargo install cargo-fuzz
```

## Commands

```bash
# Fuzz (seeds from seed_corpus/smelt_loop/)
cargo +nightly fuzz run smelt_loop -- -max_len=4096 -max_total_time=300

# Minimize the corpus after a run
cargo +nightly fuzz cmin smelt_loop seed_corpus/smelt_loop

# Crash → JSON
cargo run --bin crash_to_scenario -- artifacts/smelt_loop/<file> out.json

# Headless replay (CI / regression checks; exits non-zero on failure)
cargo run --bin replay_scenario -- out.json

# Visual replay (step-by-step in real terminal)
cargo run --bin play_scenario -- examples/hello_agent.json
```

`play_scenario` controls: `space`/`→` next, `b`/`←` back, `r` reset, `s` state dump, `q`/`Esc` quit.
