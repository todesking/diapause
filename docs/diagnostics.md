# Diagnostics style guide

How user-facing error messages of `#[diapause::coroutine]` are written.
"User-facing" means every `syn::Error` the macro can emit for code a user
might plausibly write; internal invariant violations (`panic!`/`expect`
tagged `BUG:`) are exempt.

## Message shape

A message is one sentence chain, lowercase, no trailing period, with
semicolons separating its parts in this order:

1. **What is wrong**, anchored on tokens the user wrote. Lead with the
   thing the span points at: the binding (`` `x` is bound by ... ``), the
   construct (`yield_! in a match guard ...`), or the shadowing name
   (`` this `x` shadows ... ``).
2. **Why**, when the restriction is not self-evident (e.g. "the iterator
   would be stored in the coroutine state alongside `v` itself, making the
   state self-referential"). Skip this part when the what already implies
   it.
3. **The workaround**, whenever one exists (see below).

## Vocabulary

Use these terms consistently; do not invent synonyms:

| Concept | Term |
| --- | --- |
| a value is live across a yield | "held across yield_!" |
| where suspended values are stored | "the coroutine state" |
| the program point of a suspension | "suspension point" |
| a value that must become a state field | "stored in the coroutine state" |

Write `yield_!` / `yield_all!` bare (no backticks); they are the macro
names as the user types them. Never leak implementation jargon: no CFG,
block, variant, state boundary, hoisting, or synthesized names
(`__iter0`, `__dg0`, ...) in a message.

For errors rustc itself would emit in plain Rust, mirror rustc's wording
so users recognize them: "use of undeclared label `'a`", "`break` outside
of a loop", "identifier `a` is bound more than once in the argument
list".

## Workarounds

- Every restriction that has a mechanical rewrite states it, as
  imperative advice ending in a code snippet in backticks.
- The canonical rewrite for position restrictions is binding the resume
  value first: `` `let r = yield_!(..);` `` — reuse this exact shape.
- For missing-type errors, show the annotation with the user's own
  binding name and the `Type` placeholder: `` `let x: Type = ...` ``. For
  unannotatable patterns, suggest a rebind derived from the name:
  `` `let x2: Type = x;` ``.
- Name ambiguity errors (shadowing, collisions) end with "rename ..."
  naming which binding to rename.
- Restrictions with no mechanical rewrite (e.g. a fundamental
  self-reference) still say what to do instead ("iterate by value
  instead").

## Spans

- Point at the smallest thing the user must change: the binding
  identifier, the offending sub-expression, the `unsafe` keyword — not
  the enclosing statement, unless the whole statement must be
  restructured (value-position yield).
- Never point at synthesized code. When an error concerns a synthesized
  binding (a `for` iterator, a `yield_all!` delegate), span the user
  expression it came from and describe it in words ("this `for` loop's
  iterator", "the coroutine delegated to by yield_all!").
- Multiple independent errors are all reported (combined via
  `syn::Error::combine`), not just the first.

## Errors left to rustc

Some restrictions are cheaper to enforce by generating code that fails
to compile than by checking them in the macro — but only when the
resulting error lands on the user's own tokens. `yield_all!`'s call
operand is the case to keep in mind: the delegate's type is derived from
the callee path (`sub(x)` -> `sub::State`), so a callee that is not a
coroutine leaves `sub::State` unresolved, and a coroutine taking a
reference leaves its state's lifetime unspecified. Both errors are
rustc's (E0433 / E0106), both point at the callee the user wrote, and
both are answered by the two-line form
(`let sub: SubTy = ..; yield_all!(sub)`), which the macro's own operand
error already spells out. Such cases still get a trybuild fixture, so
the message a user actually sees is recorded.

## Testing

- Every user-facing diagnostic has a trybuild case under
  `diapause/tests/compile_fail/` capturing the full message and span.
  Regenerate expected output with `TRYBUILD=overwrite cargo test` after
  any wording change.
- Unit tests assert on stable substrings of a message, not the full
  text, so rewording stays cheap; keep the asserted substring the part
  that identifies the diagnostic.
