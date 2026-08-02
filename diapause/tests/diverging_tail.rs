//! Deliberately diverging completion expressions (regression tests).
//!
//! A coroutine that is never resumed past a suspension point ends its
//! body with `unreachable!()` (or a `-> !` helper). The generated code
//! binds the completion value first (`let __ret: T = ..`), which puts
//! that expression in value position, where
//! `clippy::diverging_sub_expression` used to fire on the user's own
//! span and break `-D warnings` builds. The file-level `deny` below is
//! the regression check: it fails under `cargo clippy` if the generated
//! binding stops suppressing the lint.
#![deny(clippy::diverging_sub_expression)]

use diapause::{Coroutine, CoroutineState};

/// The trailing expression of the body diverges: control is expected to
/// never come back from this yield.
#[diapause::coroutine(yield = u32, resume = u32)]
fn never_resumed(n: u32) -> u32 {
    let _r = yield_!(n);
    unreachable!("this coroutine is never resumed")
}

#[test]
fn a_diverging_tail_expression_compiles_and_runs() {
    let mut c = never_resumed(7);
    assert_eq!(c.start(), CoroutineState::Yielded(7));
}

fn bail() -> ! {
    panic!("bail")
}

/// The divergence is hidden behind a user function, so no syntactic
/// check could spot it.
#[diapause::coroutine(yield = u32, resume = u32)]
fn diverging_helper_tail(n: u32) -> u32 {
    let _r = yield_!(n);
    bail()
}

#[test]
fn a_diverging_helper_call_as_the_tail_compiles() {
    let mut c = diverging_helper_tail(1);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
}

/// Statement-position `return` is a `Return` terminator too, so a
/// diverging returned expression lands in the same binding.
#[diapause::coroutine(yield = u32, resume = u32)]
fn diverging_return(n: u32) -> u32 {
    if n == 0 {
        let _r = yield_!(0);
        return unreachable!("resumed after the final yield");
    }
    let r = yield_!(n);
    r + 1
}

#[test]
fn a_diverging_statement_return_compiles() {
    let mut c = diverging_return(0);
    assert_eq!(c.start(), CoroutineState::Yielded(0));

    let mut c = diverging_return(3);
    assert_eq!(c.start(), CoroutineState::Yielded(3));
    assert_eq!(c.resume(4), CoroutineState::Complete(5));
}

/// The `let ... else` fall-through (a `Unreachable` terminator) is a
/// separate code path with the same hazard; it is emitted bare.
#[diapause::coroutine(yield = u32, resume = u32)]
fn let_else_diverges(opt: Option<u32>) -> u32 {
    let Some(v) = opt else {
        let _r = yield_!(404);
        unreachable!("never resumed after the failure yield")
    };
    let v2: u32 = v;
    let r = yield_!(v2);
    v2 + r
}

#[test]
fn a_diverging_let_else_block_compiles() {
    let mut c = let_else_diverges(Some(5));
    assert_eq!(c.start(), CoroutineState::Yielded(5));
    assert_eq!(c.resume(3), CoroutineState::Complete(8));

    let mut c = let_else_diverges(None);
    assert_eq!(c.start(), CoroutineState::Yielded(404));
}

/// A panic in the completion expression must still leave the state
/// poisoned rather than `Done`: the `State::Done` assignment stays
/// after the binding.
#[diapause::coroutine(yield = u32, resume = u32)]
fn panicking_tail(n: u32) -> u32 {
    let _r = yield_!(n);
    bail()
}

#[test]
fn a_panic_in_the_completion_expression_poisons_the_state() {
    let mut c = panicking_tail(1);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    let msg = *std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.resume(0)))
        .unwrap_err()
        .downcast::<&str>()
        .unwrap();
    assert_eq!(msg, "bail");

    let msg = *std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.resume(0)))
        .unwrap_err()
        .downcast::<&str>()
        .unwrap();
    assert_eq!(msg, "Poisoned");
}
