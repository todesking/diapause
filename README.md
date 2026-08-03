# diapause

[![crates.io](https://img.shields.io/crates/v/diapause.svg)](https://crates.io/crates/diapause)
[![docs.rs](https://img.shields.io/docsrs/diapause)](https://docs.rs/diapause)
[![CI](https://github.com/todesking/diapause/actions/workflows/ci.yml/badge.svg)](https://github.com/todesking/diapause/actions/workflows/ci.yml)
![MSRV](https://img.shields.io/crates/msrv/diapause)
![license](https://img.shields.io/crates/l/diapause.svg)

Coroutines/generators for stable Rust via code transformation — no
`async`, no `Pin`, no allocation, no unsafe code.

The `#[diapause::coroutine]` attribute rewrites a function into a state
machine enum: the body is analyzed as a control-flow graph and each
`yield_!` suspension point becomes an enum variant holding the live
variables.

Try it in your browser: the
[playground](https://todesking.github.io/diapause/) shows the expanded
code and control-flow graph for any annotated function — no
installation required.

```rust
use diapause::{Coroutine, CoroutineState};

#[diapause::coroutine(yield = u32, resume = u32)]
fn running_total(n: u32) -> u32 {
    let mut sum: u32 = 0;
    for i in 0u32..n {
        let bonus = yield_!(sum);
        sum += i + bonus;
    }
    sum
}

fn main() {
    let mut c = running_total(3);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(10), CoroutineState::Yielded(10));
    assert_eq!(c.resume(0), CoroutineState::Yielded(11));
    assert_eq!(c.resume(5), CoroutineState::Complete(18));
}
```

Calling the annotated function returns the initial state without running
any code. `start()` runs the body up to the first `yield_!`; each
`resume(value)` continues from the previous suspension point with
`value` as the result of the `let x = yield_!(..)` binding.

## Features

- **Control flow**: `yield_!` works inside `if` / `if let` / `match` /
  `loop` / `while` / `while let` / `for`, and inside the diverging block
  of `let ... else`, at any nesting depth, mixed with `break`,
  `continue` (including labeled forms), early `return`, and the `?`
  operator on `Result` and `Option`. The generated `resume` is a
  dispatch loop over basic blocks, so join points are never duplicated.
- **Value-producing control flow**: a yield-containing `if` / `match` /
  `loop` / block may initialize a `let` binding
  (`let x: T = if c { yield_!(1); a } else { b };` — the annotation is
  required when the value crosses the join) or stand as the function's
  trailing expression, including `break` with a value from such a
  `loop`.
- **Expression-position yield with a pure prefix**: a `yield_!` inside
  an expression is hoisted into its own `let __tmpN = yield_!(..);`
  statement when everything evaluated before it is a path, a literal,
  or another `yield_!` — `f(yield_!(1), yield_!(2), g())`,
  `yield_!(1) + 2`, `x = f(yield_!(1));`, `x += yield_!(..);`, a
  trailing `yield_!(e)` (evaluating to the resume value),
  `if f(yield_!(1)) { .. }`, `match g(yield_!(1)) { .. }`, and
  `for x in g(yield_!(1)) { .. }` all work. Effectful or panicking
  code evaluated before the yield would be reordered across the
  suspension by the hoist, so such positions remain errors (see
  Constraints).
- **Resume arguments** are ordinary values passed to `resume`, typed via
  the attribute (`resume = String`), with a separate zero-argument
  `start()` so no resume value can be silently dropped on the first
  call.
- **Delegation**: `yield_all!(sub)` runs another coroutine to
  completion — every value it yields is forwarded to the caller, every
  resume value is forwarded back in, and the expression evaluates to
  its completion value (Python's `yield from`). The inner state enum is
  stored by value inside the outer one — no boxing — so `Clone` and
  serde derives compose across arbitrary nesting depth. The operand is
  a variable holding the coroutine or a direct call of a coroutine
  function (`yield_all!(sub(x))`), whose delegate type is derived from
  the callee.
  `yield_all_resume!(sub, rv)` delegates to a coroutine that is already
  started (e.g. one deserialized mid-run), entering with `resume(rv)`
  instead of `start()`. A `?` applied to the delegation
  (`let v: T = yield_all!(sub)?;`) unwraps the completion value and
  exits early on Err. The `box` modifier (`yield_all!(box sub)`)
  stores the delegate boxed instead, enabling *recursive* delegation;
  boxing is lazy — a delegate that completes on entry never
  allocates — and requires the `alloc` feature (on by default, the only
  part of the crate that can allocate).
- **Generics, where clauses, reference arguments, and `impl Trait`
  arguments** are carried over to the generated state enum. Elided
  lifetimes are named automatically.
- **Destructuring argument patterns** (`fn f((a, b): (u32, u32))`,
  struct patterns, `_`, `ref`, `@`): the value is stored in the state
  under a fresh name and the pattern is rebound at the top of the
  body. A component crossing a yield needs an annotated rebind (see
  Constraints).
- **Snapshots**: `#[derive(Clone)]` written under the attribute is moved
  to the state enum, so a suspended coroutine can be cloned and both
  copies resumed independently.
- **Suspended-state persistence**: because the state enum stores only
  concrete types (a `for` loop's iterator is stored as
  `<T as IntoIterator>::IntoIter`, not boxed), serde derives work with
  their ordinary semantics. A suspended coroutine can be serialized,
  shipped to another process, deserialized, and resumed — something
  that is fundamentally impossible for async-based generator crates.
  An opt-in `fingerprint` flag detects a persisted state meeting
  edited source instead of resuming at a wrong program point.
- **In-place resume**: when every suspension reachable from a resume
  point leads back to the same state variant (the typical
  `loop { yield_!(x); .. }` hot path), the generated `resume` mutates
  the stored variables through `&mut self` instead of moving the whole
  enum out and back — the cost of resuming does not scale with the
  size of the state, and large buffers held across yields run at
  handwritten speed (see [docs/benchmarks.md](docs/benchmarks.md)).
  Ineligible shapes fall back to the move-out codegen automatically;
  `in_place = false` opts a coroutine out entirely (see Panic safety).
- **Panic safety**: a panicking coroutine is never left in a state that
  is unsafe to touch. On an in-place resume path (see above) a panic
  leaves the partially updated `Suspended` state behind — resuming it
  is memory-safe but unspecified; everywhere else the state becomes
  `Poisoned` and panics on further use. `in_place = false` restores
  the unconditional `Poisoned` guarantee.
- **`no_std` compatible**: the runtime crate has no `std` dependencies
  and works in `no_std` environments. No allocation, no unsafe code.

## Delegating to a sub-coroutine

`yield_all!` composes coroutines without giving up any of the state
machinery: a coroutine suspended inside a delegation is still a plain
enum value, holding the inner coroutine's state in place.

```rust
use diapause::{Coroutine, CoroutineState};
use serde::{Deserialize, Serialize};

#[diapause::coroutine(yield = u32, resume = u32)]
#[derive(Serialize, Deserialize)]
fn chunk(start: u32) -> u32 {
    let a = yield_!(start);
    start + a
}

#[diapause::coroutine(yield = u32, resume = u32)]
#[derive(Serialize, Deserialize)]
fn totals(n: u32) -> u32 {
    // Delegating to a fresh `chunk`: the delegate's type is derived
    // from the callee (`chunk::State`). Equivalently, in two lines:
    // `let sub: chunk::State = chunk(n); let first: u32 = yield_all!(sub);`
    let first: u32 = yield_all!(chunk(n)); // run a `chunk` to completion
    let again = yield_!(first);
    first + again
}

fn main() {
    let mut c = totals(5);
    assert_eq!(c.start(), CoroutineState::Yielded(5)); // chunk's yield

    // Suspended inside the delegation: the outer state holds the inner state as
    // an ordinary nested value, so persistence works across the nesting.
    let json = serde_json::to_string(&c).unwrap();
    let mut restored: totals::State = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.resume(1), CoroutineState::Yielded(6)); // chunk completes with 5 + 1
    assert_eq!(restored.resume(2), CoroutineState::Complete(8));
}
```

## Persisting a suspended coroutine

```rust
use diapause::{Coroutine, CoroutineState};
use serde::{Deserialize, Serialize};

#[diapause::coroutine(yield = u32)]
#[derive(Serialize, Deserialize)]
fn countdown(n: u32) -> u32 {
    let mut sum: u32 = 0;
    for i in 0u32..n {
        yield_!(i);
        sum += i;
    }
    sum
}

fn main() {
    let mut c = countdown(3);
    assert_eq!(c.start(), CoroutineState::Yielded(0));

    // Persist mid-iteration: the state holds the Range cursor and sum.
    let json = serde_json::to_string(&c).unwrap();

    // Elsewhere, later: restore and resume.
    let mut restored: countdown::State = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.resume(()), CoroutineState::Yielded(1));
    assert_eq!(restored.resume(()), CoroutineState::Yielded(2));
    assert_eq!(restored.resume(()), CoroutineState::Complete(3));
}
```

Serde support follows directly from what the state stores:

- Range-based iteration (`for i in 0u32..n`) round-trips fully — `Range`
  has serde impls and the mid-iteration cursor is plain data.
- Iterators without serde impls (closures, `map` adapters,
  `vec::IntoIter`, …) fail at the derive bound, as they would in any
  struct.
- A coroutine with `impl Trait` arguments has an unnameable state type,
  so it can be serialized but not deserialized.
- Variant names (`S1..`, `B1..`) are assigned in yield and block
  order; editing the coroutine body can renumber them, so persisted
  states are only compatible with the exact source they were built
  from. Restoring a state into edited source can succeed structurally
  and silently resume at the wrong program point — the `fingerprint`
  flag below exists to catch exactly this.

### Detecting stale states: `fingerprint`

Adding `fingerprint` to the attribute stamps every state with a hash of
the coroutine's source and validates it before resuming:

```rust
use diapause::{Coroutine, CoroutineState, Fingerprinted};
use serde::{Deserialize, Serialize};

#[diapause::coroutine(yield = u32, fingerprint)]
#[derive(Serialize, Deserialize)]
fn countdown(n: u32) -> u32 {
    let mut sum: u32 = 0;
    for i in 0u32..n {
        yield_!(i);
        sum += i;
    }
    sum
}

fn main() {
    let mut c = countdown(3);
    c.start();
    let json = serde_json::to_string(&c).unwrap();

    let mut restored: countdown::State = serde_json::from_str(&json).unwrap();
    // Err(diapause::FingerprintMismatch { .. }) if the state was
    // persisted by a different version of `countdown`.
    restored.check_fingerprint().unwrap();
    assert_eq!(restored.resume(()), CoroutineState::Yielded(1));
}
```

- Every state enum has a `State::FINGERPRINT: u64` associated const,
  flag or not: an FNV-1a hash of the attribute arguments, signature,
  and body tokens. Editing the coroutine changes it; comments and
  formatting do not.
- The `fingerprint` flag additionally stores that hash in every state
  as a plain `__fp: u64` field — any derive (serde, `Clone`, ...)
  handles it as ordinary data — makes `start()`/`resume()` panic on a
  mismatch as a last line of defense, and implements the
  `diapause::Fingerprinted` trait, whose
  `fn check_fingerprint(&self) -> Result<(), diapause::FingerprintMismatch>`
  validates gracefully right after deserializing.
- Enabling the flag is itself a breaking change for previously
  persisted states: they lack the `__fp` field and fail to deserialize.
- `fingerprint = "some-tag"` hashes the tag instead of the source: an
  escape hatch to declare states compatible across an edit you know
  preserves the state layout (e.g. resuming states persisted before a
  hot fix).
- The hash is computed from the macro's token-stream stringification,
  which is stable in practice but not formally guaranteed across
  rustc/proc-macro2 versions. Treat the fingerprint as a best-effort
  guard against accidental skew, not a versioned format.

## Constraints

The macro works purely syntactically — it never sees rustc's type
information — and every state transition is a compile-time rewrite.
This shows up as the following rules; each unsupported construct
produces a dedicated compile error with the workaround in the message.

- **`yield_!` needs a hoistable position**: `yield_!(expr);`,
  `let x = yield_!(expr);`, or an expression position where everything
  evaluated before the yield (in Rust's evaluation order) is a path, a
  literal, or another `yield_!` — the yield is then hoisted into a
  `let` in front of the statement. It cannot follow effectful or
  panicking code in the same statement (`f(g(), yield_!(1))`,
  `f() + yield_!(1)` — bind the yield with a `let` first), sit in a
  conditionally evaluated position (`c && yield_!(1)`, match guards,
  `if`/`match` arms nested inside an expression, closures), in a
  `while` condition or `while let` scrutinee (re-evaluated every
  iteration), in method-call arguments (receiver autoderef may run
  user `Deref` code first), in `unsafe` blocks, or inside other macro
  invocations. Yield-containing control flow produces a value only as
  a whole `let` initializer or as the function's trailing expression
  (see Features); in any other expression position, assign into an
  `Option` in each branch and `unwrap()` after the join.
- **Let chains are unsupported**: an `if`/`while` whose body contains a
  `yield_!` cannot use an edition-2024 let chain
  (`if let P = e && cond`); use nested `if let` or `match` instead.
- **`yield_all!` takes a variable or a coroutine call, not an arbitrary
  expression**: the state stores the inner coroutine, so its type must
  be spellable — either from a variable with a syntactically known type
  (`let sub: chunk::State = chunk(..); ... yield_all!(sub)`) or from the
  callee of a direct call, which is turned into the state type
  (`yield_all!(chunk(n))` stores a `chunk::State`, `f::<u32>(x)` an
  `f::State<u32>`; `use m::f as g;` works too, since the import brings
  both the function and the module of the same name into scope). A
  callee that is not a coroutine leaves `f::State` unresolved — a
  compile error, never a silently wrong program. Two cases still need
  the two-line form: a generic coroutine whose type parameters the call
  does not spell (add a turbofish, or annotate the binding), and a
  coroutine taking a reference, whose state is generic over a lifetime
  that the outer state cannot elide (`let sub: chunk::State<'a> = ..`).
  Anything else — a method call, a call through a qualified path, a
  computed callee — must be bound first. Its yield and resume types
  must match the outer coroutine's; mismatches surface as ordinary type
  errors. The inner coroutine must not have been started yet (`start()`
  panics otherwise); `yield_all_resume!(sub, rv)` enters a started one
  instead, and takes a variable only — a freshly called coroutine is
  never started — with a resume value that may be any expression not
  containing `yield_!`. Supported positions are
  the same as for value-producing control flow: a statement
  (`yield_all!(sub);`, completion value discarded), a whole `let`
  initializer, or a trailing expression — of the function body, or of
  a block, `if`/`else` branch, or match arm that is itself in one of
  these positions, recursively
  (`let v: u32 = match x { A => yield_all!(sub), _ => 0 };`) — in each
  case optionally followed by `?` (`let v: T = yield_all!(sub)?;` unwraps
  the completion value, exiting early on Err). The completion binding
  needs no annotation: its type is derived from the operand as
  `<SubTy as Coroutine<R>>::Return`.
- **Value bindings crossing the join need an annotation**: in
  `let x = if c { yield_!(1); a } else { b };` the join is a state
  variant storing `x`, so write `let x: T = ...` (the usual
  annotate-the-type error otherwise).
- **Syntactic types**: every variable held across a `yield_!` needs a
  syntactically determinable type: an explicit annotation, a suffixed
  or unambiguous literal (`123u8`, `true`), a move from a known
  variable, a function argument, or a range with known endpoints
  (`0u32..n`). Pattern bindings — match arms, `if let`, `while let`,
  `let ... else`, destructuring `for`, destructuring argument
  patterns — have nowhere to write a type annotation, so if they cross
  a yield, rebind first: `let v2: Type = v;` right after they are
  bound.
- **A `let ... else` block containing a `yield_!` must still diverge**,
  but the macro cannot make rustc check that across suspension points;
  a non-diverging block panics at run time when it falls through
  instead of failing to compile.
- **Borrows**: references are never stored in the state (the state
  machine is always `Unpin`; there is no self-reference). A direct
  borrow (`let y = &x;` / `let y = &mut x;`) crossing a yield is
  reconstructed after resume; other reference-holding values crossing a
  yield are compile errors. A `for` loop cannot iterate over a borrow
  of a local (`for x in &local`) — iterate by value, or borrow an
  argument.
- **Jumps out of suspending loops**: a `break`/`continue` targeting a
  loop that contains a `yield_!` works from anywhere in the loop body
  (a plain `if done { break; }` after a yield is fine), with two rules:
  `break` with a value can target such a loop only when the loop is a
  `let` initializer or the function's trailing expression, and a jump
  from inside a yield-free statement moves the variables of the target
  state by name, so a binding declared in that same statement that
  shadows one of them is rejected — rename the inner binding.
- **`?` is supported on `Result` and `Option` only.** It desugars to
  calls on the internal `Try` / `FromResidual` traits (visible
  in error messages when `?` is used on other types); implementing them
  for custom types is not supported.
- **Visibility**: the generated state enum is as public as the function,
  so argument and return types must be at least that visible or rustc
  reports `E0446` (private type in public interface).
- **In-place resumes access stored variables through references** (see
  Features): code on such a hot path that moves a stored variable out
  and re-initializes it before the next yield (`let old = s;` on a
  non-`Copy` `s` that is later re-assigned) can fail with `E0507`; use
  `mem::take` / `mem::replace`, or opt out with `in_place = false`.
- Variables not carried into the next state are dropped at the
  transition, which can be earlier than the end of their lexical scope.
  On an in-place resume path, values that die on a completion path are
  dropped at the state's move-out instead, which can be slightly later
  within the same `resume` call.

## Comparison with existing crates

Crates such as [genawaiter](https://crates.io/crates/genawaiter)
implement generators on stable Rust by driving an `async` block and
smuggling values through a shared cell. diapause generates the state
machine itself instead:

- no `Pin`: the generated states are plain enums and always `Unpin`;
- resume arguments are real arguments, not channel tricks;
- the state enum is an inspectable, nameable type that supports
  `derive`d snapshots and serde persistence of suspended coroutines;
- the trade-off is that the body must stick to the syntactic rules
  above, whereas async-based generators accept arbitrary control flow.
