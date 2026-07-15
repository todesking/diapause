//! Procedural macro implementation for the `baregen` crate.
//!
//! Users should depend on `baregen`, which re-exports the attribute.

use proc_macro::TokenStream;

mod analyze_cfg;
mod args;
mod expand;
mod lower;

/// Transforms a function into a coroutine state machine.
///
/// ```ignore
/// #[baregen::coroutine(yield = u32, resume = String)]
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
/// `Return` type is the function's return type.
///
/// `#[derive(...)]` attributes written **below** this attribute are
/// moved onto the generated `State` enum; other attributes (doc
/// comments etc.) stay on the starter fn.
///
/// # Supported control flow
///
/// `yield_!` may appear inside `if`, `match`, `loop`, `while`,
/// `while let`, and `for` at any nesting depth, combined with `break`,
/// `continue` (including labeled forms), early `return`, and the `?`
/// operator on `Result` and `Option`. Statements that contain no
/// `yield_!` are kept verbatim; only yield-containing control flow is
/// expanded into state-machine transitions.
///
/// # Constraints
///
/// The transformation is purely syntactic, which imposes the following
/// rules. Each violation is a dedicated compile error that names the
/// workaround.
///
/// - **Statement-position yield.** `yield_!` is only accepted as
///   `yield_!(expr);` or `let x = yield_!(expr);`. It cannot appear in
///   expressions, value-position control flow
///   (`let x = if c { yield_!(1); a } else { b };`), tail expressions,
///   conditions, match scrutinees or guards, `for`-head expressions,
///   assignments (`x = yield_!(..)` — resume values bind via `let`
///   only), `if let` (use `match`), `unsafe` blocks, or other macro
///   invocations. If you need the value in expression position, assign
///   into an `Option<T>` in each branch and `unwrap()` after the join.
/// - **Syntactic types.** A variable held across a `yield_!` must have
///   a syntactically determinable type: an explicit annotation, a
///   suffixed or unambiguous literal (`123u8`, `true`), a move from a
///   known variable, a function argument, or a range with a known
///   endpoint (`0u32..n`). This includes the iterator of a `for` loop
///   whose body yields: iterate over something with a known type.
/// - **Pattern bindings crossing a yield.** Match-arm bindings and
///   destructuring `for`-pattern bindings have nowhere to write a type
///   annotation, so they must not cross a yield; rebind first
///   (`let v2: Type = v;`) at the top of the arm or loop body.
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
///   the internal `BareTry`/`BareFromResidual` traits (they appear in
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
#[proc_macro_attribute]
pub fn coroutine(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = syn::parse_macro_input!(item as syn::ItemFn);
    expand::expand(attr.into(), item)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
