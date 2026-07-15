//! Coroutines/generators via code transformation, without `async`.
//!
//! Annotate a function with [`macro@coroutine`] and it is rewritten into
//! a state machine implementing the [`Coroutine`] trait. Suspension
//! points are written as `yield_!(expr)`; the transformation is a plain
//! AST rewrite — no `async`, no allocation, no unsafe code.
//!
//! ```
//! use baregen::{Coroutine, CoroutineState};
//!
//! #[baregen::coroutine(yield = u32, resume = u32)]
//! fn running_total(start: u32) -> u32 {
//!     let a = yield_!(start);
//!     let b = yield_!(start + a);
//!     start + a + b
//! }
//!
//! let mut c = running_total(100);
//! assert_eq!(c.start(), CoroutineState::Yielded(100));
//! assert_eq!(c.resume(1), CoroutineState::Yielded(101));
//! assert_eq!(c.resume(2), CoroutineState::Complete(103));
//! ```
//!
//! Calling the annotated function returns the initial state without
//! running any code; [`Coroutine::start`] runs the body up to the first
//! `yield_!`, and each [`Coroutine::resume`] continues from the previous
//! suspension point, passing its argument as the value of the
//! `let x = yield_!(..)` binding.
//!
//! # v1 limitations
//!
//! - `yield_!` may only appear as a top-level statement of the function
//!   body (`yield_!(expr);` or `let x = yield_!(expr);`) — not inside
//!   expressions or control flow (`if`/`match`/`loop`/`while`/`for`).
//!   Control-flow support is planned for v2.
//! - The macro never sees rustc's type information, so a variable held
//!   across a `yield_!` must have a syntactically determinable type: a
//!   type annotation (`let x: T = ..;`), a suffixed or unambiguous
//!   literal (`123u8`, `true`), a move from a known variable, or a
//!   function argument.
//! - References are never stored in the state. A direct borrow
//!   (`let y = &x;` / `let y = &mut x;`) held across a `yield_!` is
//!   reconstructed after resume instead; anything else holding a
//!   reference across a suspension point is a compile error.
//!
//! # Comparison with async-based generators
//!
//! Crates like `genawaiter` implement generators by driving an `async`
//! block. baregen instead generates the state machine itself, which
//! means no `Pin` (states are always `Unpin`), resume arguments that
//! are plain function arguments rather than shared-cell tricks, an
//! inspectable state enum that supports `#[derive(Clone)]` snapshots —
//! at the price of the v1 restrictions above.

pub use baregen_macro::coroutine;

/// Runs the README's code examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../../README.md")]
struct ReadmeDoctests;

/// The result of a [`Coroutine::start`] / [`Coroutine::resume`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoroutineState<Y, R> {
    /// The coroutine suspended at a `yield_!` with this value.
    Yielded(Y),
    /// The coroutine ran to completion and returned this value.
    Complete(R),
}

/// A resumable computation.
///
/// Implemented by the state enums that [`macro@coroutine`] generates.
/// `R` is the resume argument type (the attribute's `resume = ..`,
/// defaulting to `()`).
///
/// Unlike nightly's `std::ops::Coroutine`, `resume` takes `&mut self`
/// without `Pin`: borrows are never stored in the state, so the
/// generated state machines are always `Unpin`.
pub trait Coroutine<R = ()> {
    /// The type of values passed out at each suspension point (the
    /// attribute's `yield = ..`, defaulting to `()`).
    type Yield;
    /// The type the coroutine returns on completion, taken from the
    /// annotated function's return type.
    type Return;

    /// Runs the coroutine until the next suspension point or completion.
    ///
    /// `resume` becomes the value of the `let x = yield_!(..)` binding
    /// the coroutine is currently suspended at.
    ///
    /// # Panics
    ///
    /// Panics if the coroutine has not been started (`"Not started"`),
    /// has already completed (`"Already done"`), or panicked during a
    /// previous transition (`"Poisoned"`).
    fn resume(&mut self, resume: R) -> CoroutineState<Self::Yield, Self::Return>;

    /// Starts the coroutine, running it until the first suspension point
    /// or completion.
    ///
    /// `start` takes no resume argument because there is no `yield_!`
    /// that the first resume value could correspond to.
    ///
    /// # Panics
    ///
    /// Panics if the coroutine has already been started
    /// (`"Already started"`).
    fn start(&mut self) -> CoroutineState<Self::Yield, Self::Return>;
}

/// Desugaring target for `?` inside a `#[baregen::coroutine]` function.
///
/// The coroutine transformation rewrites `expr?` into a `branch` call so
/// that `?` works on stable without `std::ops::Try`. Supported operand
/// types are `Result` and `Option`, exactly as with `?` in a plain
/// function. This trait shows up in rustc error messages when `?` is
/// used on an unsupported type, but it is an internal implementation
/// detail: implementing it for other types is not supported.
#[doc(hidden)]
pub trait BareTry {
    type Output;
    type Residual;
    fn branch(self) -> core::ops::ControlFlow<Self::Residual, Self::Output>;
}

/// Companion of [`BareTry`]: rebuilds the coroutine's return value from
/// the residual carried out by an early-exiting `?`.
///
/// Like `BareTry`, this is an internal implementation detail that only
/// exists in error messages; implementing it is not supported.
#[doc(hidden)]
pub trait BareFromResidual<R> {
    fn from_residual(r: R) -> Self;
}

impl<T, E> BareTry for Result<T, E> {
    type Output = T;
    type Residual = E;
    fn branch(self) -> core::ops::ControlFlow<E, T> {
        match self {
            Ok(v) => core::ops::ControlFlow::Continue(v),
            Err(e) => core::ops::ControlFlow::Break(e),
        }
    }
}

// The `From` bound mirrors `?`'s error conversion in plain functions.
impl<T, E, E2> BareFromResidual<E2> for Result<T, E>
where
    E: From<E2>,
{
    fn from_residual(r: E2) -> Self {
        Err(E::from(r))
    }
}

impl<T> BareTry for Option<T> {
    type Output = T;
    type Residual = ();
    fn branch(self) -> core::ops::ControlFlow<(), T> {
        match self {
            Some(v) => core::ops::ControlFlow::Continue(v),
            None => core::ops::ControlFlow::Break(()),
        }
    }
}

impl<T> BareFromResidual<()> for Option<T> {
    fn from_residual((): ()) -> Self {
        None
    }
}

/// Marks a suspension point inside a `#[baregen::coroutine]` function.
///
/// `yield_!(expr)` suspends the coroutine, yielding `expr` to the caller.
/// `let r = yield_!(expr);` additionally binds the value passed to the
/// next `resume` call. `yield_!()` yields `()`.
///
/// It is a macro rather than the `yield` keyword because stable rustfmt
/// and IDEs handle reserved-keyword expressions poorly.
///
/// This macro is consumed by the `#[coroutine]` transformation and never
/// expands on its own; it exists so that `yield_!` resolves for IDEs and
/// documentation. Using it outside a `#[baregen::coroutine]` function is
/// a compile error.
#[macro_export]
macro_rules! yield_ {
    ($($tt:tt)*) => {
        ::core::compile_error!("yield_! may only be used inside a #[baregen::coroutine] function")
    };
}
