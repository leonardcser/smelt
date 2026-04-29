## Commands

```bash
# build
cargo build

# test (requires `cargo install cargo-nextest` — much faster than `cargo test`)
cargo nextest run --workspace

# format and lint
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings
```

Whenever you add a new user-facing feature or change user-facing behavior,
update the README.md and the docs/ folder. Don't document internal
implementation details — only things end users need to know.

## Refactor docs (`refactor/`)

This workspace is in the middle of a multi-phase architectural refactor. Before
making non-trivial changes to `crates/ui/` or `crates/tui/`, orient yourself in
the refactor docs — they describe target intent, sequencing, and the per-file
fate of every source file.

| File                                          | Read it when…                                                              |
| --------------------------------------------- | -------------------------------------------------------------------------- |
| `refactor/README.md`                          | First touch. Meta-rules, doc-sync rule, cold-start checklist.              |
| `refactor/STATUS.md`                          | Every session start — one-screen "where we are" + what's next.             |
| `refactor/REFACTOR.md`                        | Planning a phase. Sequencing of P0..P7 and each sub-phase's content.       |
| `refactor/ARCHITECTURE.md`                    | Deciding what shape something should take. Target intent + rationale.      |
| `refactor/INVENTORY.md`                       | "What happens to file X?" — per-file ledger with phase + status.           |
| `refactor/FEATURES.md`                        | Walking user-facing parity at a phase boundary.                            |
| `refactor/P0.md`, `P1.md`, …                  | What shipped + decisions made in each closed/in-progress phase.            |
| `refactor/DECISIONS.md`                       | Pre-P0 architectural decisions (frozen). Later decisions live in `P<n>.md`.|
| `refactor/TRACE.md`                           | A vertical slice walked end-to-end through the target architecture.        |
| `refactor/TESTING.md`                         | Three-layer testing strategy (model state / engine integration / render).  |
| `refactor/tui-ui-architecture-target.puml`    | Canonical structure diagram (regenerate SVG after edits).                  |
| `refactor/tui-ui-architecture-target.svg`     | Rendered diagram. Generated; don't edit by hand.                           |
| `refactor/check.sh`                           | Drift-detection invariants. Run at session start + before declaring done.  |

**Doc sync rule:** when intent or structure shifts, update every affected doc
in the same change. A phase is not "landed" until `P<n>.md` is written and the
companion docs reflect what happened.
