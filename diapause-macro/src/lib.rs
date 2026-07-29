//! Procedural macro shim for the `diapause` crate.
//!
//! The transformation itself lives in `diapause-macro-core`; this crate
//! only adapts it to the proc-macro interface. Users should depend on
//! `diapause`, which re-exports the attribute.
#![warn(missing_docs)]

use proc_macro::TokenStream;

/// Transforms a function into a coroutine state machine.
///
/// ```ignore
/// #[diapause::coroutine(yield = u32, resume = String)]
/// fn foo(a: u32) -> usize {
///     let r = yield_!(a + 1);
///     r.len()
/// }
/// ```
///
/// The attribute generates, in place of the function:
///
/// - a starter fn with the same name, visibility, and arguments that
///   returns the initial state (`foo::State`) without running any code;
/// - a module named after the function containing the `State` enum
///   (`Start`, one variant per suspension point, `Done`, and a
///   `Poisoned` placeholder) and its `Coroutine` implementation.
///
/// Attribute arguments `yield = Type` and `resume = Type` set the
/// yielded and resume-argument types; both default to `()`. The
/// `Return` type is the function's return type. The `fingerprint`
/// flag stamps every state with a hash of the coroutine's source and
/// validates it before resuming (see "State-variant naming and serde"
/// below); `fingerprint = "tag"` hashes the tag instead.
/// `in_place = false` disables the in-place resume optimization (see
/// "In-place resume and panic behavior" below).
///
/// `#[derive(...)]` attributes written **below** this attribute are
/// moved onto the generated `State` enum; other attributes (doc
/// comments etc.) stay on the starter fn.
///
/// # Supported control flow
///
/// `yield_!` may appear inside `if`, `if let`, `match`, `loop`,
/// `while`, `while let`, and `for`, and inside the diverging block of
/// `let ... else`, at any nesting depth, combined with `break`,
/// `continue` (including labeled forms), early `return`, and the `?`
/// operator on `Result` and `Option`. Statements that contain no
/// `yield_!` are kept verbatim; only yield-containing control flow is
/// expanded into state-machine transitions.
///
/// Yield-containing control flow may also produce a value in two
/// positions: as a whole `let` initializer
/// (`let x: T = if c { yield_!(1); a } else { b };`, including `break`
/// with a value from a `loop` initializer) and as the function's
/// trailing expression. The `let` form needs its type annotation
/// whenever the value crosses the join into a state variant.
///
/// A `yield_!` inside an expression is hoisted into its own
/// `let __tmpN = yield_!(..);` statement when everything evaluated
/// before it is a path, a literal, or another `yield_!`: call
/// arguments (`f(yield_!(1), yield_!(2), g())`), binary operands
/// (`yield_!(1) + 2`), assignment right-hand sides
/// (`x = f(yield_!(1));`, `x += yield_!(..);`), trailing expressions
/// (`yield_!(e)` evaluates to the resume value), `if` conditions,
/// `match`/`if let`/`let ... else` scrutinees, and `for` heads.
///
/// `yield_all!(sub)` delegates to the coroutine held by the variable
/// `sub`: its yields are forwarded to the caller, resume values are
/// forwarded back in, and the expression evaluates to its completion
/// value. Its state enum is stored by value inside the outer one, so
/// `Clone` and serde derives compose across the nesting. Supported in
/// statement position, as a whole `let` initializer, and as the
/// function's trailing expression (see the constraint below).
/// `yield_all_resume!(sub, rv)` is the same delegation for a coroutine
/// that is already started: it enters with `resume(rv)` instead of
/// `start()` and forwards from there. Both macros accept a `box`
/// modifier (`yield_all!(box sub)`) that stores the delegate boxed —
/// required for recursive delegation, where by-value storage would be
/// infinitely sized. Boxing is lazy (a delegate that completes on entry
/// never allocates) and needs diapause's `alloc` feature (a default
/// feature).
///
/// # Constraints
///
/// The transformation is purely syntactic, which imposes the following
/// rules. Each violation is a dedicated compile error that names the
/// workaround.
///
/// - **Hoistable-position yield.** `yield_!` is accepted as
///   `yield_!(expr);`, `let x = yield_!(expr);`, or in an expression
///   position where everything evaluated before it is a path, a
///   literal, or another `yield_!` (it is then hoisted into a `let` in
///   front of the statement). It cannot follow effectful or panicking
///   code in the same statement (`f(g(), yield_!(1))` — bind the yield
///   first), sit in a conditionally evaluated position
///   (`c && yield_!(1)`, guards, nested `if`/`match` arms, closures),
///   a `while` condition or `while let` scrutinee (re-evaluated every
///   iteration), method-call arguments (receiver autoderef may run
///   user `Deref` code first), `unsafe` blocks, or other macro
///   invocations. Yield-containing control flow produces a value only
///   in the two supported positions above; anywhere else, assign into
///   an `Option<T>` in each branch and `unwrap()` after the join.
/// - **No let chains.** An `if`/`while` whose body contains `yield_!`
///   cannot use an edition-2024 let chain (`if let P = e && cond`);
///   use nested `if let` or `match` instead.
/// - **`yield_all!` takes a variable.** The delegated coroutine is
///   stored in the state, so the operand must be a variable with a
///   syntactically known type; bind it first
///   (`let sub: Ty = make_sub(..);`). Its yield and resume types must
///   match the outer coroutine's (mismatches are ordinary type errors)
///   and it must not have been started yet (`start()` panics
///   otherwise; delegate to a started coroutine with
///   `yield_all_resume!(sub, rv)`, whose resume value may be any
///   expression not containing `yield_!`).
/// - **`break` with a value** may target a yield-containing loop only
///   when the loop is a `let` initializer or the function's trailing
///   expression.
/// - **Syntactic types.** A variable held across a `yield_!` must have
///   a syntactically determinable type: an explicit annotation, a
///   suffixed or unambiguous literal (`123u8`, `true`), a move from a
///   known variable, a function argument, or a range with a known
///   endpoint (`0u32..n`). This includes the iterator of a `for` loop
///   whose body yields: iterate over something with a known type.
/// - **Pattern bindings crossing a yield.** Bindings of match arms,
///   `if let` / `while let` / `let ... else` patterns, destructuring
///   `for` patterns, and destructuring argument patterns have nowhere
///   to write a type annotation, so they must not cross a yield;
///   rebind first (`let v2: Type = v;`) right after they are bound.
/// - **`let ... else` divergence is not compile-checked** once the
///   block contains a `yield_!`: a non-diverging `else` block panics
///   at run time when it falls through instead of failing to compile.
/// - **Borrows.** References are never stored in the state. A direct
///   borrow (`let y = &x;` / `let y = &mut x;`) crossing a yield is
///   dropped and reconstructed after resume; any other
///   reference-holding value crossing a yield is a compile error. A
///   `for` loop whose body yields cannot iterate over a borrow of a
///   local variable (`for x in &local`) — the stored iterator would be
///   self-referential; iterate by value or borrow an argument.
/// - **Jumps out of suspending loops.** A `break`/`continue` targeting
///   a yield-containing loop must itself sit in a yield-containing
///   statement; a plain `if done { break; }` after a yield is rejected.
///   Move the exit condition into the loop header via a flag variable.
///   `break` with a value cannot target such a loop.
/// - **`?` on `Result` and `Option` only.** The rewrite goes through
///   the internal `Try`/`FromResidual` traits (they appear in
///   rustc error messages when `?` is applied to another type);
///   implementing them for custom types is not supported.
/// - **All paths returning.** If every live path ends in an explicit
///   `return` and the diverging control flow contains yields, the
///   unreachable implicit `()` tail triggers
///   `E0308: expected <ret>, found ()`. Append `unreachable!()` as the
///   tail expression.
/// - **Visibility.** The state enum is as public as the function, so
///   argument and return types must be at least that visible
///   (otherwise `E0446`).
/// - **Drop timing.** Variables not stored in the next state are
///   dropped at the transition, possibly before their lexical scope
///   ends.
///
/// # In-place resume and panic behavior
///
/// When every suspension reachable from a resume point re-enters the
/// same state variant (the typical `loop { yield_!(..); .. }` hot
/// path), the generated `resume` updates the stored variables through
/// `&mut self` instead of moving the whole enum out and back, so the
/// cost of resuming does not scale with the size of the state (a
/// `[u64; 32]` buffer held across a yield resumes at handwritten
/// speed). Ineligible shapes — several suspension points reachable
/// from one resume, loops between two suspensions, shadowing of stored
/// names, macro invocations mentioning stored variables — fall back to
/// the always-correct move-out code automatically. The state enum's
/// variants, fields, and serde representation are identical either
/// way.
///
/// Two visible consequences, both removable with `in_place = false`:
///
/// - **Panic behavior.** Outside in-place resumes, a panic in user
///   code leaves the state `Poisoned` (further use panics). Inside
///   one, the panic leaves the partially updated `Suspended` state
///   behind instead: `status()` still reports `Suspended`, and
///   resuming is memory-safe but its behavior is unspecified
///   (variables mutated before the panic keep their new values). Code
///   that catches panics and must not observe a half-updated coroutine
///   should set `in_place = false` or drop the coroutine after a
///   caught panic.
/// - **Moves of stored variables.** In-place resumes reach stored
///   variables through references, so code that moves one out and
///   re-initializes it before the next yield
///   (`let old = s; s = rebuild(old);` on a non-`Copy` `s`) can fail
///   with `E0507: cannot move out of ...`. Use
///   `core::mem::take`/`core::mem::replace`, or `in_place = false`.
///
/// # State-variant naming and serde
///
/// Suspension points become variants `S1..Sn` in source order of their
/// yields; internal join/loop-header blocks become `B1..Bm` in reverse
/// post order. The numbering is deterministic for a given body but can
/// change when the body is edited, so serialized suspended states are
/// only compatible with the exact source they were produced from.
/// Deserializing requires naming the state type, which is impossible
/// for coroutines with `impl Trait` arguments (their state type has an
/// inferred parameter).
///
/// Every state enum carries an associated const
/// `State::FINGERPRINT: u64`: an FNV-1a hash of the attribute
/// arguments, signature, and body tokens, which changes when the
/// coroutine is edited (comments and formatting do not affect it).
/// With the `fingerprint` attribute flag, each data-carrying variant
/// additionally stores that hash in a plain `__fp: u64` field —
/// derives persist it as ordinary data — `start`/`resume` panic when
/// it does not match the current source, and the state enum implements
/// the `diapause::Fingerprinted` trait, whose
/// `check_fingerprint(&self) -> Result<(), FingerprintMismatch>` method
/// validates it gracefully right after deserializing. Enabling the
/// flag invalidates previously persisted states (missing `__fp`
/// field). `fingerprint = "tag"` hashes the tag instead of the
/// source, declaring states persisted under equal tags compatible.
#[proc_macro_attribute]
pub fn coroutine(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = syn::parse_macro_input!(item as syn::ItemFn);
    diapause_macro_core::expand(attr.into(), item)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
