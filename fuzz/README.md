# smelt fuzz

## Setup

```bash
rustup toolchain install nightly
cargo install cargo-fuzz
```

## Commands

```bash
# Fuzz
cargo +nightly fuzz run smelt_loop -- -max_len=4096 -max_total_time=300

# Crash → JSON
cargo run --bin crash_to_scenario -- artifacts/smelt_loop/<file> out.json

# Replay JSON
cargo run --bin replay_scenario -- out.json
```
