#![no_std]
#![warn(missing_docs)]
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
//! The [playground](https://todesking.github.io/diapause/) runs the
//! transform in the browser, showing the expanded code and control-flow
//! graph for any annotated function.
//!
//! # Supported bodies
//!
//! `yield_!` works inside `if` / `match` / `loop` / `while` /
//! `while let` / `for` at any nesting depth, mixed with `break`,
//! `continue`, early `return`, and the `?` operator on `Result` and
//! `Option`. [`yield_all!`] delegates to another coroutine, forwarding
//! its yields and resume values ([`yield_all_resume!`] does the same
//! for one that is already started, and the `box` modifier stores the
//! delegate boxed, enabling recursion). Because the state enum stores only
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

#[cfg(feature = "alloc")]
extern crate alloc;

pub use diapause_macro::coroutine;

/// Support items for the code `#[coroutine]` generates; not public API.
#[cfg(feature = "alloc")]
#[doc(hidden)]
pub mod __private {
    pub use alloc::boxed::Box;
}

/// Runs the README's code examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../../README.md")]
struct ReadmeDoctests;

/// The result of a [`Coroutine::start`] / [`Coroutine::resume`] call.
#[must_use = "this contains the yielded or returned value; dropping it loses that value"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoroutineState<Y, R> {
    /// The coroutine suspended at a `yield_!` with this value.
    Yielded(Y),
    /// The coroutine ran to completion and returned this value.
    Complete(R),
}

/// The result of a [`Coroutine::status`] query.
///
/// Reports which of `start`/`resume` may currently be called without
/// panicking, without having to call either and risk the panic.
#[must_use = "querying the status has no effect unless the result is inspected"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoroutineStatus {
    /// Neither `start` nor `resume` has run yet. Only `start` is valid;
    /// calling `resume` panics.
    NotStarted,
    /// The coroutine is suspended at a `yield_!`. Only `resume` is
    /// valid; calling `start` panics.
    Suspended,
    /// The coroutine ran to completion. Both `start` and `resume`
    /// panic.
    Done,
    /// A previous `start`/`resume` call panicked partway through a
    /// transition. Both `start` and `resume` panic.
    ///
    /// A panic inside an in-place resume arm does *not* poison: the
    /// state stays [`Suspended`](Self::Suspended) with whatever partial
    /// updates the code made before panicking, and resuming it is
    /// memory-safe but unspecified. See the `in_place` argument of
    /// [`macro@crate::coroutine`] for the trade-off and the opt-out.
    Poisoned,
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
///
/// The conditions under which `start`/`resume` panic are documented on
/// each method (and reported by [`status`](Self::status) beforehand);
/// the panic messages themselves are not part of the stable API.
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
    /// Panics if the coroutine has not been started, has already
    /// completed, or panicked during a previous transition.
    fn resume(&mut self, resume: R) -> CoroutineState<Self::Yield, Self::Return>;

    /// Starts the coroutine, running it until the first suspension point
    /// or completion.
    ///
    /// `start` takes no resume argument because there is no `yield_!`
    /// that the first resume value could correspond to.
    ///
    /// # Panics
    ///
    /// Panics if the coroutine has already been started, has completed,
    /// or panicked during a previous transition.
    fn start(&mut self) -> CoroutineState<Self::Yield, Self::Return>;

    /// Reports which of `start`/`resume` may currently be called without
    /// panicking, so that callers can check before calling either.
    fn status(&self) -> CoroutineStatus;

    /// Whether the coroutine has been started, i.e. it is past the
    /// point where `start` may be called.
    ///
    /// Equivalent to `status() != CoroutineStatus::NotStarted`.
    fn is_started(&self) -> bool {
        self.status() != CoroutineStatus::NotStarted
    }

    /// Whether the coroutine has run to completion and returned its
    /// final value.
    ///
    /// Equivalent to `status() == CoroutineStatus::Done`.
    fn is_done(&self) -> bool {
        self.status() == CoroutineStatus::Done
    }

    /// Non-panicking [`start`](Self::start): checks [`status`](Self::status)
    /// first and returns the offending status instead of panicking when the
    /// coroutine is not in the [`NotStarted`](CoroutineStatus::NotStarted)
    /// state.
    fn try_start(&mut self) -> Result<CoroutineState<Self::Yield, Self::Return>, CoroutineStatus> {
        match self.status() {
            CoroutineStatus::NotStarted => Ok(self.start()),
            status => Err(status),
        }
    }

    /// Non-panicking [`resume`](Self::resume): checks
    /// [`status`](Self::status) first and returns the offending status
    /// instead of panicking when the coroutine is not in the
    /// [`Suspended`](CoroutineStatus::Suspended) state. The `resume` value
    /// is dropped in that case.
    fn try_resume(
        &mut self,
        resume: R,
    ) -> Result<CoroutineState<Self::Yield, Self::Return>, CoroutineStatus> {
        match self.status() {
            CoroutineStatus::Suspended => Ok(self.resume(resume)),
            status => Err(status),
        }
    }
}

/// Forwarding impl so that generic drivers can take a coroutine by
/// mutable reference, and a coroutine can be partially iterated without
/// being consumed (`for x in Iter::new(&mut c)`).
impl<C, R> Coroutine<R> for &mut C
where
    C: Coroutine<R> + ?Sized,
{
    type Yield = C::Yield;
    type Return = C::Return;

    fn resume(&mut self, resume: R) -> CoroutineState<C::Yield, C::Return> {
        (**self).resume(resume)
    }

    fn start(&mut self) -> CoroutineState<C::Yield, C::Return> {
        (**self).start()
    }

    fn status(&self) -> CoroutineStatus {
        (**self).status()
    }
}

/// Forwarding impl so a boxed coroutine can be driven directly, e.g.
/// one stored boxed to break a recursive type. Requires the `alloc`
/// feature (enabled by default).
#[cfg(feature = "alloc")]
impl<C, R> Coroutine<R> for alloc::boxed::Box<C>
where
    C: Coroutine<R> + ?Sized,
{
    type Yield = C::Yield;
    type Return = C::Return;

    fn resume(&mut self, resume: R) -> CoroutineState<C::Yield, C::Return> {
        (**self).resume(resume)
    }

    fn start(&mut self) -> CoroutineState<C::Yield, C::Return> {
        (**self).start()
    }

    fn status(&self) -> CoroutineStatus {
        (**self).status()
    }
}

/// Fingerprint validation of a persisted coroutine state.
///
/// Implemented by the state enums that [`macro@coroutine`] generates
/// when the attribute is given the `fingerprint` flag, so that generic
/// persistence layers can validate states without naming each concrete
/// state type.
pub trait Fingerprinted {
    /// The fingerprint of the current coroutine source: an FNV-1a hash
    /// of the attribute arguments, signature, and body tokens (or of the
    /// tag, with `fingerprint = "tag"`).
    const FINGERPRINT: u64;

    /// Checks that this state was created by the same coroutine source
    /// (see [`Self::FINGERPRINT`]). Call it right after deserializing to
    /// detect a mismatch gracefully; `start`/`resume` panic on the same
    /// condition. Terminal states (`Done`, `Poisoned`) carry no
    /// fingerprint and always pass.
    fn check_fingerprint(&self) -> Result<(), FingerprintMismatch>;
}

/// A persisted coroutine state does not match the source of the
/// coroutine it is checked against.
///
/// Returned by [`Fingerprinted::check_fingerprint`], which
/// [`macro@coroutine`] implements when the attribute is given the
/// `fingerprint` flag. Call it right after deserializing to detect the
/// mismatch gracefully; `start`/`resume` panic on the same condition.
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

mod sealed {
    /// Seals [`Try`](super::Try) and [`FromResidual`](super::FromResidual)
    /// to `Result` and `Option`: `?` in a coroutine supports exactly the
    /// operand types `?` in a plain function does.
    pub trait Sealed {}
    impl<T, E> Sealed for Result<T, E> {}
    impl<T> Sealed for Option<T> {}
}

/// Desugaring target for `?` inside a `#[diapause::coroutine]` function.
///
/// The coroutine transformation rewrites `expr?` into a `branch` call so
/// that `?` works on stable without `std::ops::Try`. Supported operand
/// types are `Result` and `Option`, exactly as with `?` in a plain
/// function. This trait shows up in rustc error messages when `?` is
/// used on an unsupported type, but it is an internal implementation
/// detail: it is sealed and cannot be implemented for other types.
#[doc(hidden)]
pub trait Try: sealed::Sealed {
    type Output;
    type Residual;
    fn branch(self) -> core::ops::ControlFlow<Self::Residual, Self::Output>;
}

/// Companion of [`Try`]: rebuilds the coroutine's return value from
/// the residual carried out by an early-exiting `?`.
///
/// Like `Try`, this is an internal implementation detail that only
/// exists in error messages; it is sealed and cannot be implemented
/// for other types.
#[doc(hidden)]
pub trait FromResidual<R>: sealed::Sealed {
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

/// A wrapper that implements `Iterator` for a coroutine.
///
/// Converts a coroutine with `resume = ()` into an `Iterator` that yields
/// the coroutine's yielded values. The coroutine's completion value is
/// discarded.
///
/// A coroutine whose `resume` type is `()` also implements
/// [`IntoIterator`] directly (the `#[coroutine]` macro generates it), so
/// the state can be passed straight to a `for` loop without wrapping it in
/// `Iter::new`. `Iter::new` remains as the general entry point, e.g. when
/// naming the iterator type or converting a value already held as `C`;
/// `Iter::new(&mut c)` borrows the coroutine instead of consuming it
/// (via the `Coroutine for &mut C` forwarding impl), so
/// `for x in Iter::new(&mut c)` iterates partially and leaves the rest
/// resumable.
///
/// This wrapper does not implement `Deref`/`DerefMut`; access the wrapped
/// coroutine explicitly through [`get_ref`](Self::get_ref),
/// [`get_mut`](Self::get_mut), or [`into_inner`](Self::into_inner). The
/// iterator holds no shadow state: `next` decides what to do by asking the
/// coroutine's [`status`](Coroutine::status), so driving the coroutine
/// directly through `get_mut` stays consistent with continued iteration.
///
/// # Example: Direct iteration
///
/// ```
/// use diapause::Coroutine;
///
/// #[diapause::coroutine(yield = u32, resume = ())]
/// fn count_up() {
///     let nums: [u32; 3] = [0, 1, 2];
///     for i in nums {
///         yield_!(i);
///     }
/// }
///
/// let mut iter = diapause::Iter::new(count_up());
/// assert_eq!(iter.next(), Some(0));
/// assert_eq!(iter.next(), Some(1));
/// assert_eq!(iter.next(), Some(2));
/// assert_eq!(iter.next(), None);
/// ```
///
/// # Example: Using a for loop
///
/// A `resume = ()` coroutine implements `IntoIterator`, so it can be
/// passed directly to `for`:
///
/// ```
/// #[diapause::coroutine(yield = u32, resume = ())]
/// fn count_to_three() {
///     let nums: [u32; 3] = [1, 2, 3];
///     for n in nums {
///         yield_!(n);
///     }
/// }
///
/// let mut sum = 0;
/// for n in count_to_three() {
///     sum += n;
/// }
/// assert_eq!(sum, 6);
/// ```
pub struct Iter<C> {
    coroutine: C,
}

impl<C> Iter<C> {
    /// Creates a new iterator from a coroutine.
    pub const fn new(coroutine: C) -> Self {
        Iter { coroutine }
    }

    /// Returns a shared reference to the wrapped coroutine.
    pub fn get_ref(&self) -> &C {
        &self.coroutine
    }

    /// Returns a mutable reference to the wrapped coroutine.
    ///
    /// Driving the coroutine through this reference stays consistent with
    /// continued iteration: `next` re-derives what to do from the
    /// coroutine's [`status`](Coroutine::status) rather than any state
    /// cached in the `Iter`.
    pub fn get_mut(&mut self) -> &mut C {
        &mut self.coroutine
    }

    /// Consumes the iterator and returns the wrapped coroutine.
    pub fn into_inner(self) -> C {
        self.coroutine
    }
}

impl<C> Iterator for Iter<C>
where
    C: Coroutine<()>,
{
    type Item = C::Yield;

    /// Drives the coroutine to its next suspension point and returns the
    /// yielded value, or `None` once it completes (the completion value
    /// is discarded).
    ///
    /// # Panics
    ///
    /// Panics if the coroutine panicked during a previous transition
    /// (i.e. its [`status`](Coroutine::status) is
    /// [`Poisoned`](CoroutineStatus::Poisoned)).
    fn next(&mut self) -> Option<Self::Item> {
        let step = match self.coroutine.status() {
            CoroutineStatus::NotStarted => self.coroutine.start(),
            CoroutineStatus::Suspended => self.coroutine.resume(()),
            CoroutineStatus::Done => return None,
            CoroutineStatus::Poisoned => panic!("Poisoned"),
        };
        match step {
            CoroutineState::Yielded(y) => Some(y),
            CoroutineState::Complete(_) => None,
        }
    }
}

impl<C> core::iter::FusedIterator for Iter<C> where C: Coroutine<()> {}

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
/// started yet (`start` panics otherwise) — delegate to an
/// already-started coroutine with [`yield_all_resume!`].
///
/// The operand is either a variable whose type is syntactically known
/// (bind the coroutine with a type annotation first) or a direct call
/// of a coroutine function — `yield_all!(sub(x))`, whose delegate type
/// is derived from the callee path as `sub::State`, carrying a
/// turbofish over as its arguments (`sub::<u32>(x)` gives
/// `sub::State<u32>`). Two shapes are not derivable and need the
/// variable form: a generic coroutine called without a turbofish, and
/// one taking a reference (its state is generic over a lifetime the
/// outer state cannot elide). Any other expression is a compile error.
/// Supported positions are a
/// statement (`yield_all!(sub);`, completion value discarded), a whole
/// `let` initializer (`let x = yield_all!(sub);` — the binding's type
/// is derived from the operand, an explicit annotation wins), and a
/// trailing expression: the function body's, or that of a block,
/// `if`/`else` branch, or match arm that is itself in one of these
/// positions, recursively
/// (`let v: u32 = match x { A => yield_all!(sub), _ => 0 };`). In each
/// position the delegation may be followed by `?`
/// (`let v: T = yield_all!(sub)?;`), unwrapping the completion value
/// and exiting early on Err.
///
/// The inner coroutine's state enum is stored by value inside the outer
/// one, so `Clone` and serde derives compose: a coroutine suspended
/// inside a delegation serializes with the nested state included.
///
/// # Boxed delegation
///
/// `yield_all!(box sub)` stores the delegate boxed instead, which is
/// what makes *recursive* delegation representable — by-value storage
/// of a state inside itself would be infinitely sized. Boxing is lazy:
/// the delegate is started unboxed and boxed only if it actually
/// suspends, so a delegate that completes on entry never allocates.
/// Requires the `alloc` feature (enabled by default); the contract and
/// supported positions are otherwise identical to the unboxed form.
///
/// ```
/// use diapause::{Coroutine, CoroutineState};
///
/// #[diapause::coroutine(yield = u32)]
/// fn countdown(n: u32) {
///     yield_!(n);
///     if n > 0 {
///         yield_all!(box countdown(n - 1));
///     }
/// }
///
/// # fn main() {
/// let mut c = countdown(1);
/// assert_eq!(c.start(), CoroutineState::Yielded(1));
/// assert_eq!(c.resume(()), CoroutineState::Yielded(0));
/// assert_eq!(c.resume(()), CoroutineState::Complete(()));
/// # }
/// ```
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

/// Delegates to an already-started coroutine inside a
/// `#[diapause::coroutine]` function.
///
/// `yield_all_resume!(sub, rv)` resumes the suspended coroutine held by
/// the variable `sub` with the resume value `rv`, then runs it to
/// completion exactly like [`yield_all!`]: every value it yields is
/// yielded to the caller, every resume value is forwarded back into it,
/// and the whole expression evaluates to its completion value. Where
/// `yield_all!` enters via [`Coroutine::start`] and requires a coroutine
/// that has not been started, `yield_all_resume!` enters via
/// [`Coroutine::resume`] and requires one that is suspended at a
/// `yield_!` (`resume` panics otherwise).
///
/// ```
/// use diapause::{Coroutine, CoroutineState};
///
/// #[diapause::coroutine(yield = u32, resume = u32)]
/// fn running_total(start: u32) -> u32 {
///     let a = yield_!(start);
///     let b = yield_!(start + a);
///     start + a + b
/// }
///
/// #[diapause::coroutine(yield = u32, resume = u32)]
/// fn outer(sub: running_total::State, rv: u32) -> u32 {
///     yield_all_resume!(sub, rv)
/// }
///
/// # fn main() {
/// let mut sub = running_total(100);
/// // The first yield is consumed here, before delegation begins.
/// assert_eq!(sub.start(), CoroutineState::Yielded(100));
/// let mut c = outer(sub, 1);
/// assert_eq!(c.start(), CoroutineState::Yielded(101));
/// assert_eq!(c.resume(2), CoroutineState::Complete(103));
/// # }
/// ```
///
/// The first operand must be a variable whose type is syntactically
/// known — [`yield_all!`]'s call form has no meaning here, since a
/// freshly called coroutine is never started. The resume value may be
/// any expression
/// not containing `yield_!`; it is consumed by the first `resume` call
/// before any suspension and is never stored in the state. The
/// supported positions are the same: a statement
/// (`yield_all_resume!(sub, rv);`, completion value discarded), a whole
/// `let` initializer, and a trailing expression (of the function body,
/// or of a block, `if`/`else` branch, or match arm in one of these
/// positions), each optionally followed by `?`. The `box` modifier
/// (`yield_all_resume!(box sub, rv)`) stores the delegate boxed with
/// the same lazy-boxing behavior as [`yield_all!`]'s (see *Boxed
/// delegation* there; requires the `alloc` feature).
///
/// Like [`yield_!`], this macro is consumed by the `#[coroutine]`
/// transformation and never expands on its own; using it outside a
/// `#[diapause::coroutine]` function is a compile error.
#[macro_export]
macro_rules! yield_all_resume {
    ($($tt:tt)*) => {
        ::core::compile_error!(
            "yield_all_resume! may only be used inside a #[diapause::coroutine] function"
        )
    };
}
