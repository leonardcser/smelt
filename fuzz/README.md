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
