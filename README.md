# baregen

Coroutines/generators for stable Rust via code transformation — no
`async`, no `Pin`, no allocation, no unsafe code.

The `#[baregen::coroutine]` attribute rewrites a function into a state
machine enum: the body is analyzed as a control-flow graph and each
`yield_!` suspension point becomes an enum variant holding the live
variables.

```rust
use baregen::{Coroutine, CoroutineState};

#[baregen::coroutine(yield = u32, resume = u32)]
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
- **Resume arguments** are ordinary values passed to `resume`, typed via
  the attribute (`resume = String`), with a separate zero-argument
  `start()` so no resume value can be silently dropped on the first
  call.
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
- **Panic safety**: a coroutine that panics mid-transition is left in a
  `Poisoned` state and panics on further use.

## Persisting a suspended coroutine

```rust
use baregen::{Coroutine, CoroutineState};
use serde::{Deserialize, Serialize};

#[baregen::coroutine(yield = u32)]
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
  states are only compatible with the exact source they were built from.

## Constraints

The macro works purely syntactically — it never sees rustc's type
information — and every state transition is a compile-time rewrite.
This shows up as the following rules; each unsupported construct
produces a dedicated compile error with the workaround in the message.

- **`yield_!` is statement-position only**: `yield_!(expr);` or
  `let x = yield_!(expr);`. It cannot appear inside expressions
  (`f(yield_!(1))`, `1 + yield_!(2)`), conditions, match scrutinees or
  guards (including `if let` / `while let` / `let ... else`
  scrutinees), `for`-head expressions, assignments (`x = yield_!(..)` —
  resume values bind via `let` only), or `unsafe` blocks.
  Yield-containing control flow produces a value only as a whole `let`
  initializer or as the function's trailing expression (see Features);
  in any other expression position, assign into an `Option` in each
  branch and `unwrap()` after the join.
- **Let chains are unsupported**: an `if`/`while` whose body contains a
  `yield_!` cannot use an edition-2024 let chain
  (`if let P = e && cond`); use nested `if let` or `match` instead.
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
  loop that contains a `yield_!` must sit inside a statement that also
  contains a `yield_!`; a plain `if done { break; }` after a yield is
  an error. Move the exit condition into the loop header via a flag
  variable, or restructure so the jump shares a statement with a yield.
  `break` with a value can target such a loop only when the loop is a
  `let` initializer or the function's trailing expression.
- **`?` is supported on `Result` and `Option` only.** It desugars to
  calls on the internal `BareTry` / `BareFromResidual` traits (visible
  in error messages when `?` is used on other types); implementing them
  for custom types is not supported.
- **A body whose every reachable path ends in an explicit `return`** —
  with the diverging paths containing yields — produces a puzzling
  `E0308: expected <ret>, found ()` on the unreachable implicit tail.
  Append `unreachable!()` as the tail expression.
- **Visibility**: the generated state enum is as public as the function,
  so argument and return types must be at least that visible or rustc
  reports `E0446` (private type in public interface).
- Variables not carried into the next state are dropped at the
  transition, which can be earlier than the end of their lexical scope.

## Comparison with existing crates

Crates such as [genawaiter](https://crates.io/crates/genawaiter)
implement generators on stable Rust by driving an `async` block and
smuggling values through a shared cell. baregen generates the state
machine itself instead:

- no `Pin`: the generated states are plain enums and always `Unpin`;
- resume arguments are real arguments, not channel tricks;
- the state enum is an inspectable, nameable type that supports
  `derive`d snapshots and serde persistence of suspended coroutines;
- the trade-off is that the body must stick to the syntactic rules
  above, whereas async-based generators accept arbitrary control flow.
