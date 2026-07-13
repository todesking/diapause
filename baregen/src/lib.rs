//! Coroutines/generators via code transformation, without `async`.
//!
//! Annotate a function with [`macro@coroutine`] to turn it into a state
//! machine implementing the [`Coroutine`] trait. Suspension points are
//! written as `yield_!(expr)`.

pub use baregen_macro::coroutine;

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
/// Unlike nightly's `std::ops::Coroutine`, `resume` takes `&mut self`
/// without `Pin`: borrows are never stored in the state, so the generated
/// state machines are always `Unpin`.
pub trait Coroutine<R = ()> {
    type Yield;
    type Return;

    /// Runs the coroutine until the next suspension point or completion.
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
    /// Panics if the coroutine has already been started.
    fn start(&mut self) -> CoroutineState<Self::Yield, Self::Return>;
}

/// Marks a suspension point inside a `#[baregen::coroutine]` function.
///
/// `yield_!(expr)` suspends the coroutine, yielding `expr` to the caller.
/// `let r = yield_!(expr);` additionally binds the value passed to the
/// next `resume` call.
///
/// This macro is consumed by the `#[coroutine]` transformation and never
/// expands on its own; it exists so that `yield_!` resolves for IDEs and
/// documentation. Using it outside a `#[baregen::coroutine]` function is
/// a compile error.
#[macro_export]
macro_rules! yield_ {
    ($($tt:tt)*) => {
        ::core::compile_error!(
            "yield_! may only be used inside a #[baregen::coroutine] function"
        )
    };
}
