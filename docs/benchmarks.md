# Benchmarks

Quantitative comparison of `#[diapause::coroutine]`-generated state
machines against [genawaiter] 0.99.1 (an async/await-based generator
crate) and handwritten state machines, along three axes: runtime
throughput, generated code size, and compile time.

[genawaiter]: https://crates.io/crates/genawaiter

## Measurement conditions

- Date: 2026-07-23, diapause commit `a855b5e`
- Machine: Apple M4 (10 cores: 4P + 6E), 16 GB RAM, macOS 26.5.2
  (`aarch64-apple-darwin`)
- Toolchain: rustc 1.96.0 (ac68faa20 2026-05-25), stable
- Profiles: Cargo defaults (`bench`/`release` = opt-level 3, no LTO;
  `dev` = opt-level 0)
- Harness: criterion 0.7.0, default settings (100 samples, 3 s warm-up,
  5 s measurement)

## Workloads

Defined in [`diapause-bench/src/lib.rs`](../diapause-bench/src/lib.rs);
every workload is implemented three ways with identical observable
behavior (asserted by a unit test):

| workload | shape | yields per drive |
|---|---|---|
| `counter(1024)` | single `for` loop, `yield_!(i)` | 1024 |
| `nested(64)` | two nested `for` loops, `yield_!(i ^ j)` | 2016 |
| `running_total(1024)` | `for` loop, yields a running sum, folds the resume argument back in, returns the final sum | 1024 |

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

Each benchmark constructs the coroutine and drives it to completion
through the resume protocol, summing the yielded values.

## 1. Runtime throughput

`cargo bench -p diapause-bench` — criterion medians, time per drive and
throughput in yielded elements per second:

| workload | diapause | handwritten | genawaiter_rc | genawaiter_stack |
|---|---|---|---|---|
| `counter` | **1.67 µs** (613 M/s) | 1.71 µs (599 M/s) | 2.47 µs (415 M/s) | 1.86 µs (550 M/s) |
| `nested` | **3.63 µs** (555 M/s) | 2.16 µs (935 M/s) | 4.05 µs (498 M/s) | 4.48 µs (450 M/s) |
| `running_total` | **1.99 µs** (515 M/s) | 1.94 µs (529 M/s) | 3.96 µs (258 M/s) | 3.90 µs (262 M/s) |

Observations:

- diapause is on par with the handwritten state machine for the flat
  workloads (within a few percent, occasionally ahead) and 1.1–2×
  faster than genawaiter across the board; with resume values the gap
  to genawaiter is ~2×.
- `nested` is the one case where a human wins: the handwritten machine
  collapses both loops into a single two-counter `step()`, while the
  generated dispatch loop keeps the two `Range` iterators from the
  source. diapause still beats both genawaiter flavors there.
- ~1.6–1.9 ns per resume/yield round trip in all diapause cases: suspension
  is an enum tag write plus a dispatch on resume, no allocation
  anywhere (`rc::Gen` pays one allocation per construction, visible in
  `counter`).

## 2. Generated code size

### Macro expansion (`cargo expand -p diapause-bench <module>`)

Source vs. post-expansion size of the three workload modules
(prettyplease-formatted lines):

| module | source | expanded | notes |
|---|---|---|---|
| `dia` (diapause) | 27 lines / 0.6 kB | 439 lines / 19.7 kB | ~16× lines: 3 state enums + `Coroutine` impls with per-block dispatch |
| `hand` (handwritten) | 206 lines / 7.0 kB | 206 lines / 7.0 kB | no macros |
| `ga` (genawaiter) | 31 lines / 0.9 kB | 31 lines / 0.9 kB | async blocks expand inside the compiler, not visibly |

The generated source is ~2× the size of the handwritten equivalent of
the same three coroutines; genawaiter's expansion cost exists but is
hidden in rustc's coroutine transform.

### Machine code (release `__text` section)

Four minimal example binaries run the same three workloads
(`diapause-bench/examples/size_*.rs`); `size_baseline` has the same
I/O scaffolding but no coroutines. `cargo build --release --examples
-p diapause-bench`, then `size -m` (cargo-bloat was not available on
this machine; section-size deltas over the baseline binary serve the
same purpose):

| binary | `__text` bytes | delta over baseline | stripped file size delta |
|---|---|---|---|
| `size_baseline` | 217 552 | — | — |
| `size_diapause` | 218 684 | **+1 132** | +1 280 |
| `size_handwritten` | 218 524 | +972 | +1 360 |
| `size_genawaiter` | 221 048 | +3 496 | +17 776 |

The three diapause state machines compile to essentially the same
amount of machine code as the handwritten ones (+160 bytes across all
three workloads); the genawaiter versions cost ~3.5× more text and pull
in allocator/`Rc` machinery visible in the stripped-file delta.

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

- Proc-macro expansion (CFG analysis + state-enum generation) costs
  ~1.7 ms per coroutine in dev builds, where it dominates: a dev
  rebuild with 500 coroutines takes 0.8 s vs 0.1 s for the
  alternatives.
- In release builds codegen dominates and the gap mostly closes:
  diapause is ~20 % slower than genawaiter and ~50 % slower than
  handwritten code at N = 500 — roughly 2.4 ms per coroutine
  end-to-end.

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
