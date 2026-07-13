# baregen

Coroutines/generators for stable Rust via code transformation — no
`async`, no `Pin`, no allocation, no unsafe code.

The `#[baregen::coroutine]` attribute rewrites a function into a state
machine enum by splitting its body at each `yield_!` suspension point.

```rust
use baregen::{Coroutine, CoroutineState};

#[baregen::coroutine(yield = u32, resume = u32)]
fn running_total(start: u32) -> u32 {
    let a = yield_!(start);
    let b = yield_!(start + a);
    start + a + b
}

fn main() {
    let mut c = running_total(100);
    assert_eq!(c.start(), CoroutineState::Yielded(100));
    assert_eq!(c.resume(1), CoroutineState::Yielded(101));
    assert_eq!(c.resume(2), CoroutineState::Complete(103));
}
```

Calling the annotated function returns the initial state without running
any code. `start()` runs the body up to the first `yield_!`; each
`resume(value)` continues from the previous suspension point with
`value` as the result of the `let x = yield_!(..)` binding.

## Features

- **Resume arguments** are ordinary values passed to `resume`, typed via
  the attribute (`resume = String`), with a separate argument-less
  `start()` so the first resume value has nowhere to be lost.
- **Generics, where clauses, reference arguments, and `impl Trait`
  arguments** are carried over to the generated state enum. Elided
  lifetimes are named automatically.
- **Snapshots**: `#[derive(Clone)]` written under the attribute is moved
  to the state enum, so a suspended coroutine can be cloned and both
  copies resumed independently.
- **Panic safety**: a coroutine that panics mid-transition is left in a
  `Poisoned` state and panics on further use.

## v1 limitations

- `yield_!` may only appear as a top-level statement of the function
  body: `yield_!(expr);` or `let x = yield_!(expr);`. Yields inside
  expressions or control flow (`if` / `match` / `loop` / `while` /
  `for`) are compile errors. Control-flow support via CFG analysis is
  planned for v2.
- The macro works purely syntactically, so every variable held across a
  `yield_!` needs a syntactically determinable type: an explicit
  annotation, a suffixed or unambiguous literal (`123u8`, `true`), a
  move from a variable of known type, or a function argument. Anything
  else gets a "write a type annotation" error.
- References are never stored in the state (the state machine is always
  `Unpin`; there is no self-reference). A direct borrow (`let y = &x;`
  / `let y = &mut x;`) crossing a yield is dropped and reconstructed
  after resume; other reference-holding values crossing a yield are
  compile errors.

## Comparison with existing crates

Crates such as [genawaiter](https://crates.io/crates/genawaiter)
implement generators on stable Rust by driving an `async` block and
smuggling values through a shared cell. baregen generates the state
machine itself instead:

- no `Pin`: the generated states are plain enums and always `Unpin`;
- resume arguments are real arguments, not channel tricks;
- the state enum is an inspectable, nameable type that supports
  `derive`d snapshots;
- in exchange, only the v1 subset above is supported, whereas
  async-based generators accept arbitrary control flow.
