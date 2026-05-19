# smelt fuzz

## Setup

```bash
rustup toolchain install nightly
cargo install cargo-fuzz
tar -xzf seed_corpus.tar.gz   # warm cache for the fuzzer
```

The corpus is tracked as a single tarball; the unpacked `seed_corpus/` dir is
gitignored. After a long fuzz session, repack with:

```bash
cargo +nightly fuzz cmin smelt_loop seed_corpus/smelt_loop
tar -czf seed_corpus.tar.gz seed_corpus
```

## Commands

```bash
# Full local cycle: unpack → fuzz → cmin → repack tarball (default 300s)
cargo xtask fuzz [seconds]

# Stop-on-first-crash, parallel workers — pair with the `fuzz-triage` skill
# for the fix-as-you-go loop. For unattended runs that keep going past
# crashes, swap `-ignore_crashes=0` for `=1`.
cargo +nightly fuzz run --sanitizer=none smelt_loop -- -fork=4 -ignore_crashes=0
cargo +nightly fuzz run --sanitizer=none text_ops   -- -fork=4 -ignore_crashes=0

# Lower-level pieces:
cargo +nightly fuzz run smelt_loop -- -max_len=4096 -max_total_time=300
cargo +nightly fuzz cmin smelt_loop seed_corpus/smelt_loop

# Crash → JSON (both targets supported via --target)
cargo run --bin crash_to_scenario -- artifacts/smelt_loop/<file> out.json
cargo run --bin crash_to_scenario -- --target lua_loop artifacts/lua_loop/<file> out.json

# Structural shrinker — drops whole ops + halves string payloads until
# the minimal subset that still panics remains. Run AFTER `cargo fuzz tmin`
# for the byte-level pass; this one operates on the decoded JSON, so a
# 200-op crash typically minimizes to <10 ops in a few seconds.
cargo run --release --bin shrink_scenario -- out.json out.min.json
cargo run --release --bin shrink_scenario -- --target lua_loop out.json out.min.json

# Headless replay (CI / regression checks; exits non-zero on failure)
cargo run --bin replay_scenario -- out.json
cargo run --bin replay_scenario -- --target lua_loop out.json

# Visual replay (step-by-step in real terminal — smelt_loop only)
cargo run --bin play_scenario -- examples/hello_agent.json
```

`play_scenario` controls: `space`/`→` next, `b`/`←` back, `r` reset, `s` state dump, `q`/`Esc` quit.
