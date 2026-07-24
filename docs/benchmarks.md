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

- Date: 2026-07-25
- Code: diapause commit `47b85a3` (all three axes; earlier rounds
  referenced below: `ec10652` = before the in-place resume arms,
  `703c73a` = the commit introducing them)
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
| `counter` | **1.64 µs** | 1.69 µs | 2.26 µs | 1.84 µs | 6.13 µs | 15.6 µs | 2.16 µs |
| `nested` | 3.62 µs | **2.16 µs** | 4.02 µs | 4.31 µs | 7.90 µs | 23.2 µs | 4.34 µs |
| `running_total` | 1.99 µs | **1.96 µs** | 3.92 µs | 3.93 µs | 6.88 µs | 10.0 µs | 3.91 µs |
| `large_state` | **1.84 µs** | 1.86 µs | 2.33 µs | 1.91 µs | 6.17 µs | 15.6 µs | 2.20 µs |

Observations:

- On the small-state workloads diapause is on par with the handwritten
  state machine (within a few percent) and the fastest library
  approach: 1.1–2× faster than genawaiter and 1.2–2× faster than
  next-gen. With resume values (`running_total`) the gap to both
  async-transform crates is ~2×.
- `large_state` — previously diapause's worst case (9.95 µs at commit
  `ec10652`, 5.2× slower than handwritten: the generated `resume` moved
  the live variables out of the state enum and wrote them back at every
  transition, copying the 256-byte buffer twice per yield) — is now on
  par with the handwritten machine. The in-place resume arms introduced
  in `703c73a` bind the suspended variant's fields by `&mut` when every
  reachable suspension re-enters the same variant, so the buffer is
  updated in place exactly as a handwritten `step()` would
  (−81.6% on this workload per criterion's change report; the
  small-state workloads are unchanged within noise, −0.6…−1.5%).
- `nested` is the one small-state case where a human wins: the
  handwritten machine collapses both loops into a single two-counter
  `step()`, while the generated dispatch loop keeps the two `Range`
  iterators from the source (the resume slice re-enters the inner loop
  header from the outer body — a join, so the in-place arm does not
  apply). diapause still beats every library there.
- corosensei's rows keep swinging between measurement rounds with no
  change to its code or toolchain: −23…−29% from `ec10652` to
  `703c73a`, then +31…+46% back in this round (`counter`
  4.60 µs → 6.13 µs); genawaiter_rc has moved −14…+11% the same way —
  binary-layout sensitivity (see "Scope and caveats" below); the
  cross-implementation ordering is unaffected.
- The stackful crates are dominated by per-construction stack setup
  under this construct-and-drive protocol. Solving the fixed and
  per-yield components from `counter` (1024 yields) and `nested`
  (2016 yields, same construction): corosensei ≈ 4.3 µs construction
  + ≈1.8 ns/yield, generator ≈ 7.8 µs + ≈7.7 ns/yield. corosensei's
  steady-state resume cost is thus on par with diapause's; the table
  mostly shows its stack allocation, which stack reuse (`with_stack`,
  not measured) would amortize away for long-lived coroutines.
- next_gen tracks genawaiter_stack closely on every workload (both
  drive a compiler-generated async state machine; next-gen passes
  values through a stack slot instead of an `Rc` cell).
- ~1.6–2.0 ns per resume/yield round trip for diapause on every
  workload now that resuming no longer copies the state: suspension
  is an enum tag write (or, on an in-place arm, no state write at
  all beyond what the user code does) plus a dispatch on resume, no
  allocation anywhere. (`rc::Gen` does
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
| `dia` (diapause) | 37 lines / 0.9 kB | 689 lines / 31.7 kB | ~19× lines: 4 state enums + `Coroutine` impls with per-block dispatch; the in-place resume arms (commit `703c73a`) duplicate the hot loop bodies, +114 lines over the previous measurement |
| `hand` (handwritten) | 200 lines / 5.3 kB | 273 lines / 9.3 kB | no macros |
| `ga` (genawaiter) | 45 lines / 1.2 kB | 41 lines / 1.3 kB | async blocks expand inside the compiler, not visibly |

The generated source is ~2.5× the size of the handwritten equivalent
of the same four coroutines; genawaiter's expansion cost exists but is
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
| `size_diapause` | 218 676 | **+1 124** | +496 |
| `size_handwritten` | 218 524 | +972 | +544 |
| `size_genawaiter` | 221 048 | +3 496 | +17 008 |

The three diapause state machines compile to essentially the same
amount of machine code as the handwritten ones (+152 bytes across all
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
| 100 | dev | 0.23 | 0.07 | 0.05 |
| 100 | release | 0.34 | 0.26 | 0.19 |
| 500 | dev | 0.95 | 0.12 | 0.13 |
| 500 | release | 1.55 | 1.04 | 0.83 |

Observations:

- The dev rows measure warm *incremental* rebuilds of unchanged
  content (incremental compilation is on by default in dev): rustc's
  incremental cache skips most type-checking and codegen for every
  style, but proc-macro expansion always re-runs, so the dev gap is
  dominated by diapause's expansion (CFG analysis + state-enum
  generation) plus re-parsing/hashing the expanded output — ~1.6 ms
  per coroutine per rebuild over the alternatives.
  This is what an editor-driven rebuild loop feels like, not the cost
  of compiling changed code: with `CARGO_INCREMENTAL=0` at N = 100
  the dev gap narrows to 0.23 s (diapause) vs 0.19 s (genawaiter) vs
  0.12 s (handwritten) (measured 2026-07-24 at `ec10652`, before the
  in-place resume arms; same machine and toolchain).
- In release builds (no incremental caching by default) codegen
  dominates and the gap narrows: diapause is ~50 % slower than
  genawaiter and ~90 % slower than handwritten code at N = 500 —
  roughly 3.1 ms per coroutine end-to-end. This is up from 1.21 s /
  ~2.4 ms per coroutine at `ec10652`: the in-place resume arms
  duplicate each eligible hot loop body into a fast arm, growing both
  the expansion and the code handed to rustc (the runtime win on
  `large_state` is what this compile-time cost buys).

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
# everything below in one shot; writes raw logs plus a summary.md whose
# tables mirror this document (see --help for stage selection)
python3 diapause-bench/scripts/run_all_benchmarks.py

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
