# smelt-perf

Lightweight allocation and timing instrumentation.

Two coordinated pieces:

- `alloc::Counting`: a global allocator shim that bumps process and
  per-thread counters when `alloc::enable()` has been called. Cheap when
  disabled.
- `perf::begin`: RAII scope guards that record duration and per-thread
  alloc deltas under a `&'static str` label. Snapshot via
  `perf::snapshot`, pretty-print via `perf::print_summary`.

Designed to be embedded in any binary: install `Counting` as
`#[global_allocator]`, call `alloc::enable()` and `perf::enable()` early
in `main`, then sprinkle `let _g = perf::begin("scope.label");` around
interesting scopes.

```rust
use smelt_perf::{alloc, perf};

#[global_allocator]
static A: alloc::Counting = alloc::Counting;

fn main() {
    alloc::enable();
    perf::enable();

    let _g = perf::begin("work");
    // ... do work ...
    drop(_g);

    perf::print_summary();
}
```

Part of the [smelt](https://github.com/leonardcser/smelt) project but
usable standalone.

## License

MIT
