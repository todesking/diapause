//! Statement-position `return` as a CFG terminator (regression tests).
//!
//! A `return` after a yield used to be rewritten into an opaque
//! completion block before lowering, leaving a false fall-through edge
//! in the CFG. Two symptoms followed: variables dead on the returning
//! path were kept live across the false edge and stored in state
//! variants (producing unused-variable warnings in the generated resume
//! arms — the file-level deny below is the regression check), and a
//! `return` out of a yielding loop kept the loop backedge alive, making
//! rustc reject moves of loop-state variables with E0382 ("value moved
//! in previous iteration of loop") that plain Rust accepts.
#![deny(unused_variables)]

use diapause::{Coroutine, CoroutineState};

/// Symptom 1: `a` is dead on the returning path. With the false
/// fall-through edge it looked live at the first yield, was stored in
/// that state variant, and was unpacked unused in the resume arm.
#[diapause::coroutine(yield = u32, resume = u32)]
fn guarded(n: u32) -> u32 {
    let a: u32 = n * 2;
    if n > 0 {
        let r = yield_!(1);
        return r + 100;
    }
    let r2 = yield_!(a);
    r2
}

#[test]
fn dead_variable_on_the_returning_path_is_not_stored() {
    let mut c = guarded(5);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(2), CoroutineState::Complete(102));

    let mut c = guarded(0);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(7), CoroutineState::Complete(7));
}

/// Symptom 2: moving a loop-state variable in a branch that returns.
/// The stale backedge used to store `s` into the loop header's state
/// again after the (terminating) return, which rustc rejected with
/// E0382; with the `Return` terminator no backedge follows the move.
#[diapause::coroutine(yield = u32, resume = u32)]
fn move_out_of_loop(s: String) -> String {
    loop {
        let r = yield_!(1);
        if r == 0 {
            yield_!(2);
            return s;
        }
    }
}

#[test]
fn returning_branch_may_move_loop_state() {
    let mut c = move_out_of_loop("done".to_string());
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(3), CoroutineState::Yielded(1));
    assert_eq!(c.resume(0), CoroutineState::Yielded(2));
    assert_eq!(c.resume(9), CoroutineState::Complete("done".to_string()));
}

/// The dispatch-loop shape: a `loop { match .. }` where one arm yields
/// and another returns, moving the accumulated state out.
#[diapause::coroutine(yield = u32, resume = u32)]
fn dispatch_loop() -> Vec<u32> {
    let mut acc: Vec<u32> = Vec::new();
    loop {
        let cmd = yield_!(acc.len() as u32);
        match cmd {
            0 => return acc,
            n => {
                let m: u32 = n;
                let extra = yield_!(m);
                acc.push(m + extra);
            }
        }
    }
}

#[test]
fn match_arm_return_moves_the_accumulator() {
    let mut c = dispatch_loop();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(4), CoroutineState::Yielded(4));
    assert_eq!(c.resume(1), CoroutineState::Yielded(1));
    assert_eq!(c.resume(2), CoroutineState::Yielded(2));
    assert_eq!(c.resume(3), CoroutineState::Yielded(2));
    assert_eq!(c.resume(0), CoroutineState::Complete(vec![5, 5]));
}

/// A `return` in an opaque `if` inside a yielding loop goes through the
/// completion jump marker instead; the move is still accepted because
/// the marker's transition diverges.
#[diapause::coroutine(yield = u32, resume = u32)]
fn opaque_return_in_loop(s: String) -> String {
    loop {
        let r = yield_!(1);
        if r == 0 {
            return s;
        }
    }
}

#[test]
fn opaque_return_completes_from_inside_a_loop() {
    let mut c = opaque_return_in_loop("ok".to_string());
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(5), CoroutineState::Yielded(1));
    assert_eq!(c.resume(0), CoroutineState::Complete("ok".to_string()));
}

/// `return yield_!(..);` — the yield hoists out of the return value and
/// the return terminates the resume block.
#[diapause::coroutine(yield = u32, resume = u32)]
fn return_a_resume_value(flag: bool) -> u32 {
    if flag {
        return yield_!(1);
    }
    let r = yield_!(2);
    r + 10
}

#[test]
fn return_of_a_yield_value() {
    let mut c = return_a_resume_value(true);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(42), CoroutineState::Complete(42));

    let mut c = return_a_resume_value(false);
    assert_eq!(c.start(), CoroutineState::Yielded(2));
    assert_eq!(c.resume(1), CoroutineState::Complete(11));
}

#[derive(Debug, PartialEq)]
pub struct ParseError;

fn parse(s: &str) -> Result<u32, ParseError> {
    s.parse().map_err(|_| ParseError)
}

/// A `?` inside a statement return's value: the `?`'s exit transition is
/// synthesized while the return itself terminates the block, and the
/// value (including the `From` conversion) is evaluated before the state
/// write either way.
#[diapause::coroutine(yield = u32, resume = u32)]
fn try_in_return_value(s: &'static str, flag: bool) -> Result<u32, ParseError> {
    if flag {
        let r = yield_!(1);
        return Ok(parse(s)? + r);
    }
    let _r = yield_!(2);
    Ok(0)
}

#[test]
fn try_inside_a_return_value() {
    let mut c = try_in_return_value("40", true);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(2), CoroutineState::Complete(Ok(42)));

    let mut c = try_in_return_value("x", true);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(2), CoroutineState::Complete(Err(ParseError)));

    let mut c = try_in_return_value("ignored", false);
    assert_eq!(c.start(), CoroutineState::Yielded(2));
    assert_eq!(c.resume(0), CoroutineState::Complete(Ok(0)));
}

/// A bare `return;` in a unit coroutine terminates the path.
#[diapause::coroutine(yield = u32, resume = u32)]
fn bare_return(stop: bool) {
    if stop {
        yield_!(1);
        return;
    }
    yield_!(2);
}

#[test]
fn bare_return_completes() {
    let mut c = bare_return(true);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(0), CoroutineState::Complete(()));

    let mut c = bare_return(false);
    assert_eq!(c.start(), CoroutineState::Yielded(2));
    assert_eq!(c.resume(0), CoroutineState::Complete(()));
}
