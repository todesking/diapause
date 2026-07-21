//! Coroutines/generators via code transformation, without `async`.
//!
//! Annotate a function with [`macro@coroutine`] and it is rewritten into
//! a state machine implementing the [`Coroutine`] trait. Suspension
//! points are written as `yield_!(expr)`; the body is analyzed as a
//! control-flow graph and rewritten at compile time — no `async`, no
//! allocation, no unsafe code.
//!
//! ```
//! use diapause::{Coroutine, CoroutineState};
//!
//! #[diapause::coroutine(yield = u32, resume = u32)]
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
//! # Supported bodies
//!
//! `yield_!` works inside `if` / `match` / `loop` / `while` /
//! `while let` / `for` at any nesting depth, mixed with `break`,
//! `continue`, early `return`, and the `?` operator on `Result` and
//! `Option`. [`yield_all!`] delegates to another coroutine, forwarding
//! its yields and resume values. Because the state enum stores only
//! concrete types, serde derives work with their ordinary semantics and
//! a suspended coroutine can be serialized, deserialized elsewhere, and
//! resumed — nested delegation states included.
//!
//! The macro never sees rustc's type information and works purely
//! syntactically, which imposes rules on the body: `yield_!` is
//! statement-position only, variables held across a suspension point
//! need syntactically determinable types, and only direct borrows may
//! cross a yield. See [`macro@coroutine`] for the full constraint list
//! with workarounds.
//!
//! # Comparison with async-based generators
//!
//! Crates like `genawaiter` implement generators by driving an `async`
//! block. diapause instead generates the state machine itself, which
//! means no `Pin` (states are always `Unpin`), resume arguments that
//! are plain function arguments rather than shared-cell tricks, an
//! inspectable state enum that supports `#[derive(Clone)]` snapshots
//! and serde persistence — at the price of the syntactic rules above.

pub use diapause_macro::coroutine;

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

/// A persisted coroutine state does not match the source of the
/// coroutine it is checked against.
///
/// Returned by the `check_fingerprint` method that [`macro@coroutine`]
/// generates when the attribute is given the `fingerprint` flag. Call
/// it right after deserializing to detect the mismatch gracefully;
/// `start`/`resume` panic on the same condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FingerprintMismatch {
    /// The fingerprint of the current source (`State::FINGERPRINT`).
    pub expected: u64,
    /// The fingerprint stored in the state when it was created.
    pub found: u64,
}

impl core::fmt::Display for FingerprintMismatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "coroutine state fingerprint mismatch: expected {:#018x}, found {:#018x} \
             (the state was created by a different version of the coroutine)",
            self.expected, self.found
        )
    }
}

impl core::error::Error for FingerprintMismatch {}

/// Desugaring target for `?` inside a `#[diapause::coroutine]` function.
///
/// The coroutine transformation rewrites `expr?` into a `branch` call so
/// that `?` works on stable without `std::ops::Try`. Supported operand
/// types are `Result` and `Option`, exactly as with `?` in a plain
/// function. This trait shows up in rustc error messages when `?` is
/// used on an unsupported type, but it is an internal implementation
/// detail: implementing it for other types is not supported.
#[doc(hidden)]
pub trait Try {
    type Output;
    type Residual;
    fn branch(self) -> core::ops::ControlFlow<Self::Residual, Self::Output>;
}

/// Companion of [`Try`]: rebuilds the coroutine's return value from
/// the residual carried out by an early-exiting `?`.
///
/// Like `Try`, this is an internal implementation detail that only
/// exists in error messages; implementing it is not supported.
#[doc(hidden)]
pub trait FromResidual<R> {
    fn from_residual(r: R) -> Self;
}

impl<T, E> Try for Result<T, E> {
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
impl<T, E, E2> FromResidual<E2> for Result<T, E>
where
    E: From<E2>,
{
    fn from_residual(r: E2) -> Self {
        Err(E::from(r))
    }
}

impl<T> Try for Option<T> {
    type Output = T;
    type Residual = ();
    fn branch(self) -> core::ops::ControlFlow<(), T> {
        match self {
            Some(v) => core::ops::ControlFlow::Continue(v),
            None => core::ops::ControlFlow::Break(()),
        }
    }
}

impl<T> FromResidual<()> for Option<T> {
    fn from_residual((): ()) -> Self {
        None
    }
}

/// Marks a suspension point inside a `#[diapause::coroutine]` function.
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
/// documentation. Using it outside a `#[diapause::coroutine]` function is
/// a compile error.
#[macro_export]
macro_rules! yield_ {
    ($($tt:tt)*) => {
        ::core::compile_error!("yield_! may only be used inside a #[diapause::coroutine] function")
    };
}

/// Delegates to another coroutine inside a `#[diapause::coroutine]`
/// function (the analogue of Python's `yield from`).
///
/// `yield_all!(sub)` runs the coroutine held by the variable `sub` to
/// completion: every value it yields is yielded to the caller, every
/// resume value is forwarded back into it, and the whole expression
/// evaluates to its completion value. The inner coroutine's yield and
/// resume types must match the outer ones, and it must not have been
/// started yet (`start` panics otherwise).
///
/// The operand must be a variable whose type is syntactically known
/// (bind the coroutine with a type annotation first); passing an
/// arbitrary expression is a compile error. Supported positions are a
/// statement (`yield_all!(sub);`, completion value discarded), a whole
/// `let` initializer with a type annotation
/// (`let x: T = yield_all!(sub);`), and the function's trailing
/// expression.
///
/// The inner coroutine's state enum is stored by value inside the outer
/// one, so `Clone` and serde derives compose: a coroutine suspended
/// inside a delegation serializes with the nested state included.
///
/// Like [`yield_!`], this macro is consumed by the `#[coroutine]`
/// transformation and never expands on its own; using it outside a
/// `#[diapause::coroutine]` function is a compile error.
#[macro_export]
macro_rules! yield_all {
    ($($tt:tt)*) => {
        ::core::compile_error!(
            "yield_all! may only be used inside a #[diapause::coroutine] function"
        )
    };
}
