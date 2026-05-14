# Testing

The principles. For the rolling work plan and status, see [`TESTING_PLAN.md`](./TESTING_PLAN.md).

## Architecture

Code is split into two halves:

- **Core** — high decision density, low dependency. Pure functions: inputs → outputs, no `self`, no `tokio`, no `mlua`, no `crossterm`, no I/O. Decisions live here.
- **Ring** — high dependency, low decision density. Terminal I/O, network, Lua FFI, filesystem, async runtime. Effects live here.

The ring **uses** the core. It never embeds decisions. When you find a decision inside the ring, extract it into the core and call it from the ring.

This is the only architectural rule that matters for testability.

## What a test is

A test names one behaviour the codebase guarantees. The behaviour is a sentence a user could say:

> "When I press Escape while the picker is open, the picker closes."
> "When the buffer is empty and I press Ctrl-C, smelt quits."
> "When the LLM returns a tool call with no name, the engine emits an error event."

Each such sentence is **one** test. The test name *is* the sentence in `snake_case`. The assertion is one line.

## What we test

1. **Pure functions in the core.** Table-driven where the input space is finite.
2. **Dispatchers** — functions of shape `fn route(input, &State) -> Action`. Returns an `Action` enum naming what should happen. Tested by enumeration.
3. **State transitions** — given a state and a sequence of events, the final state matches. Built on the dispatchers.
4. **Visual output** — `insta` snapshots of the terminal grid. One per visual state, not per behaviour.
5. **CLI contract** — argv → exit code / stdout. Subprocess + `wiremock`. Kept thin.

## What we don't test

- Pure data types with only derives.
- FFI glue (crossterm read/write, mlua bindings, reqwest call sites). Test the pure side; trust the FFI.
- Generated code.
- Implementation details: private state, intermediate calls. If a test breaks under a pure refactor, the test was wrong.

## Rules

1. A test is a behaviour, not a function. Name it after what it guarantees.
2. If a test is hard to write, the code is wrong, not the test. Refactor the seam.
3. Test through the public surface. No reaching into private state.
4. Decisions are pure; effects are dumb. Dispatchers return `Action`; appliers mutate state. Test the dispatcher; snapshot the state.
5. Speed is correctness. Fast tests get run. Unit + behavioural should be <5s combined.
6. **The litmus test:** if you deleted this test, could you describe exactly what behaviour we stopped guaranteeing? If no, delete it.

## Tiers

| Tier | What                            | Speed       | Tooling                      |
|------|---------------------------------|-------------|------------------------------|
| 1    | Unit — pure functions in core   | µs          | `#[test]`                    |
| 2    | Behavioural — `route()` decisions | µs        | `#[test]`                    |
| 3    | State-transition — `TestApp`    | <10ms       | in-process driver harness    |
| 4    | Visual snapshots                | ms          | `insta` + storybook          |
| 5    | Binary integration              | ~1s         | subprocess + `wiremock`      |
| 6    | Property tests                  | ms          | `proptest`                   |
| 7    | Fuzz tests                      | continuous  | `cargo-fuzz`                 |

## Running

```bash
# all tests
cargo nextest run --workspace

# coverage report
cargo llvm-cov nextest --workspace --summary-only

# coverage for one crate, opened in browser
cargo llvm-cov nextest -p smelt-edit --html --open

# coverage with missing-line annotations
cargo llvm-cov nextest --show-missing-lines -p smelt-engine
```
