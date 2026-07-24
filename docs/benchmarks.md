# Benchmarks

Quantitative comparison of `#[diapause::coroutine]`-generated state
machines against [genawaiter] 0.99.1 (async/await-based),
[corosensei] 0.3.4 and [generator] 0.8.9 (stackful),
[next-gen] 0.1.1 (proc-macro over an async transform), and handwritten
state machines, along three axes: runtime throughput, generated code
size, and compile time.

[genawaiter]: https://crates.io/crates/genawaiter
[corosensei]: https://crates.io/crates/corosensei
[generator]: https://crates.io/crates/generator
[next-gen]: https://crates.io/crates/next-gen

## Measurement conditions

- Date: 2026-07-24 (runtime, code size); 2026-07-23/24 (compile time)
- Code: diapause commit `ec10652`
- Machine: Apple M4 (10 cores: 4P + 6E), 16 GB RAM, macOS 26.5.2
  (`aarch64-apple-darwin`)
- Toolchain: rustc 1.96.0 (ac68faa20 2026-05-25), stable
- Profiles: Cargo defaults (`bench`/`release` = opt-level 3, no LTO;
  `dev` = opt-level 0)
- Harness: criterion 0.7.0, default settings (100 samples, 3 s warm-up,
  5 s measurement)

## Workloads

Defined in [`diapause-bench/src/lib.rs`](../diapause-bench/src/lib.rs);
every workload is implemented once per approach with identical
observable behavior (asserted by a unit test):

| workload | shape | yields per drive |
|---|---|---|
| `counter(1024)` | single `for` loop, `yield_!(i)` | 1024 |
| `nested(64)` | two nested `for` loops, `yield_!(i ^ j)` | 2016 |
| `running_total(1024)` | `for` loop, yields a running sum, folds the resume argument back in, returns the final sum | 1024 |
| `large_state(1024)` | `for` loop over a `[u64; 32]` buffer that stays live across every yield; updates one slot per iteration and yields it | 1024 |

The implementations:

- **diapause**: `#[diapause::coroutine]` functions.
- **handwritten**: hand-rolled structs of live variables implementing
  `diapause::Coroutine`, with the same status bookkeeping
  (NotStarted/Suspended/Done tracking and assertions) the generated
  code performs.
- **genawaiter_rc**: `genawaiter::rc::Gen` (safe API, allocates the
  future; this is the flavor the genawaiter README leads with).
- **genawaiter_stack**: `genawaiter::stack::let_gen_using!` (no
  allocation; construction is part of the measured loop, as it is for
  the other implementations).
- **corosensei**: stackful `corosensei::Coroutine::new`; every
  construction allocates a fresh stack, which the measured loop
  includes (the crate can reuse stacks via `with_stack`; not
  measured).
- **generator**: stackful `generator::Gn::new_scoped`; every
  construction allocates a fresh stack, included in the measured loop.
- **next_gen**: `#[next_gen::generator]` functions, stack-pinned with
  `mk_gen!` (no allocation); pinning forces construction and the
  drive loop into one function, mirroring what the benches measure
  for every other implementation.

Each benchmark constructs the coroutine and drives it to completion
through the resume protocol, summing the yielded values.

## 1. Runtime throughput

`cargo bench -p diapause-bench` — criterion's reported point
estimates, time per drive (bold = fastest in row; divide the yields
per drive from the workload table by these times for throughput):

| workload | diapause | handwritten | genawaiter_rc | genawaiter_stack | corosensei | generator | next_gen |
|---|---|---|---|---|---|---|---|
| `counter` | **1.66 µs** | 1.71 µs | 2.29 µs | 1.86 µs | 6.43 µs | 15.5 µs | 2.17 µs |
| `nested` | 3.64 µs | **2.19 µs** | 4.69 µs | 4.29 µs | 8.27 µs | 22.9 µs | 4.27 µs |
| `running_total` | 2.01 µs | **1.98 µs** | 4.05 µs | 3.95 µs | 7.41 µs | 9.77 µs | 3.95 µs |
| `large_state` | 9.95 µs | **1.88 µs** | 2.08 µs | 1.91 µs | 6.58 µs | 15.9 µs | 2.20 µs |

Observations:

- On the small-state workloads diapause is on par with the handwritten
  state machine (within a few percent) and the fastest library
  approach: 1.1–2× faster than genawaiter and 1.2–2× faster than
  next-gen. With resume values (`running_total`) the gap to both
  async-transform crates is ~2×.
- `large_state` is diapause's worst case: 5.2× slower than the
  handwritten machine and ~5× slower than genawaiter/next-gen. The
  generated `resume` moves the live variables out of the state enum
  and writes them back at every transition, so the 256-byte buffer is
  copied twice per yield (\~9.7 ns per round trip vs \~1.8 ns
  handwritten); every other approach keeps the buffer in place (async
  frame, stackful stack, or struct field). This is a real cost of the
  by-value state-enum design that also enables `Unpin`, `derive`, and
  serde snapshots.
- `nested` is the one small-state case where a human wins: the
  handwritten machine collapses both loops into a single two-counter
  `step()`, while the generated dispatch loop keeps the two `Range`
  iterators from the source. diapause still beats every library there.
- The stackful crates are dominated by per-construction stack setup
  under this construct-and-drive protocol. Solving the fixed and
  per-yield components from `counter` (1024 yields) and `nested`
  (2016 yields, same construction): corosensei ≈ 4.5 µs construction
  + ≈1.9 ns/yield, generator ≈ 7.9 µs + ≈7.5 ns/yield. corosensei's
  steady-state resume cost is thus on par with diapause's; the table
  mostly shows its stack allocation, which stack reuse (`with_stack`,
  not measured) would amortize away for long-lived coroutines.
- next_gen tracks genawaiter_stack closely on every workload (both
  drive a compiler-generated async state machine; next-gen passes
  values through a stack slot instead of an `Rc` cell).
- ~1.6–2.0 ns per resume/yield round trip for diapause on the
  small-state workloads: suspension is an enum tag write plus a
  dispatch on resume, no allocation anywhere. (`rc::Gen` does
  allocate, but only once per construction, which is negligible
  amortized over 1024 yields; its gap over the `stack` flavor is
  per-yield overhead of the `Rc`-based value passing, not the
  allocation.)

## 2. Generated code size

### Macro expansion (`cargo expand -p diapause-bench <module>`)

Source vs. post-expansion size of the workload modules (all four
workloads; source = raw lines in `lib.rs`, expanded =
prettyplease-formatted `cargo expand` output, so the macro-free
modules differ only by formatting):

| module | source | expanded | notes |
|---|---|---|---|
| `dia` (diapause) | 37 lines / 0.9 kB | 575 lines / 25.7 kB | ~16× lines: 4 state enums + `Coroutine` impls with per-block dispatch |
| `hand` (handwritten) | 200 lines / 5.3 kB | 273 lines / 9.3 kB | no macros |
| `ga` (genawaiter) | 45 lines / 1.2 kB | 41 lines / 1.3 kB | async blocks expand inside the compiler, not visibly |

The generated source is ~2× the size of the handwritten equivalent of
the same four coroutines; genawaiter's expansion cost exists but is
hidden in rustc's coroutine transform.

### Machine code (release `__text` section)

Four minimal example binaries run the three small-state workloads
(`diapause-bench/examples/size_*.rs`; `large_state` and the
additional comparator crates are not part of the size probes);
`size_baseline` has the same I/O scaffolding but no coroutines.
`cargo build --release --examples -p diapause-bench`, then `size -m`;
section-size deltas over the baseline binary isolate each approach's
footprint:

| binary | `__text` bytes | delta over baseline | stripped file size delta |
|---|---|---|---|
| `size_baseline` | 217 552 | — | — |
| `size_diapause` | 218 684 | **+1 132** | +496 |
| `size_handwritten` | 218 524 | +972 | +544 |
| `size_genawaiter` | 221 072 | +3 520 | +17 008 |

The three diapause state machines compile to essentially the same
amount of machine code as the handwritten ones (+160 bytes across all
three workloads); the genawaiter versions cost ~3.5× more text (the
monomorphized `Rc`/future machinery). Note the stripped-file deltas
are quantized by Mach-O page alignment: total section growth for the
genawaiter binary is ~5.2 kB (text plus unwind tables), but its
`__TEXT` segment crosses a 16 KiB page boundary, so most of the
+17 kB file delta is alignment padding, not code. `__text` is the
meaningful column.

## 3. Compile time

`diapause-bench/scripts/compile_time_bench.py` generates a standalone
crate per approach containing N structurally identical resume-value
coroutines (only constants differ), warms the dependency build, then
times rebuilds of the leaf crate alone (`touch src/lib.rs; cargo
build`). Median of 3 rebuilds, wall-clock seconds:

| N | mode | diapause | genawaiter | handwritten |
|---|---|---|---|---|
| 100 | dev | 0.20 | 0.06 | 0.05 |
| 100 | release | 0.26 | 0.25 | 0.18 |
| 500 | dev | 0.83 | 0.12 | 0.12 |
| 500 | release | 1.21 | 1.01 | 0.81 |

Observations:

- The dev rows measure warm *incremental* rebuilds of unchanged
  content (incremental compilation is on by default in dev): rustc's
  incremental cache skips most type-checking and codegen for every
  style, but proc-macro expansion always re-runs, so the dev gap is
  dominated by diapause's expansion (CFG analysis + state-enum
  generation) plus re-parsing/hashing the expanded output — ~1.4 ms
  per coroutine per rebuild over the alternatives.
  This is what an editor-driven rebuild loop feels like, not the cost
  of compiling changed code: with `CARGO_INCREMENTAL=0` at N = 100
  the dev gap narrows to 0.23 s (diapause) vs 0.19 s (genawaiter) vs
  0.12 s (handwritten) (measured 2026-07-24, same machine and
  toolchain).
- In release builds (no incremental caching by default) codegen
  dominates and the gap mostly closes: diapause is ~20 % slower than
  genawaiter and ~50 % slower than handwritten code at N = 500 —
  roughly 2.4 ms per coroutine end-to-end.

## Scope and caveats

- These are micro-benchmarks of resume/yield overhead in isolation:
  the loop bodies are a couple of ALU ops. With real work per element
  the relative gaps shrink accordingly. `large_state` probes one
  larger-state shape (256 bytes of live state); states much larger
  than that, or non-`Copy` state, are still unmeasured.
- Differences of a few percent — e.g. diapause vs handwritten on the
  flat workloads — are within what code layout and link order alone
  can move; read those rows as "on par", not as a stable ordering.
  This is not hypothetical: adding the extra comparators elsewhere in
  the bench binary (leaving the genawaiter code untouched) shifted
  `genawaiter_rc` on `nested` from 4.05 µs to 4.69 µs (+15 %) between
  otherwise-identical runs.
- One machine, one target: numbers are from a single
  `aarch64-apple-darwin` box; magnitudes will differ on other
  microarchitectures — in particular the stackful crates' context
  switch and stack allocation costs are highly platform-dependent.
- The drive protocol constructs a fresh coroutine per iteration, so
  for the stackful crates (corosensei, generator) each iteration pays
  a stack allocation, amortized over 1024–2016 yields. Both crates
  can reuse stacks across coroutines; that configuration is not
  measured. Sections 2 and 3 (code size, compile time) still compare
  only diapause / genawaiter / handwritten.

## Reproducing

```sh
# runtime
cargo bench -p diapause-bench

# expansion size (requires cargo-expand)
cargo expand -p diapause-bench dia | wc -lc

# machine code size
cargo build --release --examples -p diapause-bench
size -m target/release/examples/size_* | grep __text

# compile time
python3 diapause-bench/scripts/compile_time_bench.py --out "$TMPDIR/cb" --n 100
```
