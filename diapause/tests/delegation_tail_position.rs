//! Delegation macros in expression-tail positions: the value context of
//! a supported position (`let` initializer, function tail, statement)
//! distributes into the tails of `if`/`match`/block expressions, so
//! `yield_all!` / `yield_all_resume!` are supported as the trailing
//! expression of a match arm, a block, or an `if`/`else` branch there —
//! recursively, and optionally followed by `?`.

use diapause::{Coroutine, CoroutineState};

#[diapause::coroutine(yield = u32, resume = u32)]
fn inner_sum(start: u32) -> u32 {
    let a = yield_!(start);
    let b = yield_!(start + a);
    start + a + b
}

/// Completes with `Err(start)` when resumed with 0, so the `?` forms
/// have an early-exit path to exercise.
#[diapause::coroutine(yield = u32, resume = u32)]
fn checked_sum(start: u32) -> Result<u32, u32> {
    let a = yield_!(start);
    if a == 0 { Err(start) } else { Ok(start + a) }
}

// === Match arm tails ===

#[diapause::coroutine(yield = u32, resume = u32)]
fn arm_tail_let(n: u32) -> u32 {
    let g: inner_sum::State = inner_sum(n);
    let v: u32 = match n {
        0 => 0,
        _ => yield_all!(g),
    };
    v + 1
}

#[test]
fn unbraced_arm_tail_as_a_let_initializer() {
    let mut c = arm_tail_let(10);
    assert_eq!(c.start(), CoroutineState::Yielded(10));
    assert_eq!(c.resume(2), CoroutineState::Yielded(12));
    assert_eq!(c.resume(3), CoroutineState::Complete(16)); // 10+2+3, +1

    // The non-delegating arm never suspends.
    let mut c = arm_tail_let(0);
    assert_eq!(c.start(), CoroutineState::Complete(1));
}

#[diapause::coroutine(yield = u32, resume = u32)]
fn arm_tail_fn(n: u32) -> u32 {
    let g: inner_sum::State = inner_sum(n);
    match n {
        0 => 0,
        _ => {
            yield_all!(g)
        }
    }
}

#[test]
fn braced_arm_tail_as_the_function_tail() {
    let mut c = arm_tail_fn(100);
    assert_eq!(c.start(), CoroutineState::Yielded(100));
    assert_eq!(c.resume(1), CoroutineState::Yielded(101));
    assert_eq!(c.resume(2), CoroutineState::Complete(103));
}

/// A brace-delimited invocation at an arm tail reaches lowering as a
/// trailing `Stmt::Macro` inside the arm's block.
#[diapause::coroutine(yield = u32, resume = u32)]
fn arm_tail_brace_form(n: u32) -> u32 {
    let g: inner_sum::State = inner_sum(n);
    match n {
        0 => 0,
        _ => {
            yield_all! { g }
        }
    }
}

#[test]
fn brace_delimited_arm_tail() {
    let mut c = arm_tail_brace_form(100);
    assert_eq!(c.start(), CoroutineState::Yielded(100));
    assert_eq!(c.resume(1), CoroutineState::Yielded(101));
    assert_eq!(c.resume(2), CoroutineState::Complete(103));
}

/// In a statement-position `match` the arm's completion value is
/// discarded, exactly like a statement-position delegation.
#[diapause::coroutine(yield = u32, resume = u32)]
fn stmt_match_arm(n: u32) -> u32 {
    let g: inner_sum::State = inner_sum(n);
    match n {
        0 => {}
        _ => {
            yield_all!(g);
        }
    }
    7
}

#[test]
fn arm_tail_in_a_statement_match_discards_the_completion_value() {
    let mut c = stmt_match_arm(10);
    assert_eq!(c.start(), CoroutineState::Yielded(10));
    assert_eq!(c.resume(2), CoroutineState::Yielded(12));
    assert_eq!(c.resume(3), CoroutineState::Complete(7));
}

// === Block and if/else tails ===

#[diapause::coroutine(yield = u32, resume = u32)]
fn block_tail_let(n: u32) -> u32 {
    let v: u32 = {
        let g: inner_sum::State = inner_sum(n);
        yield_all!(g)
    };
    v + 1
}

#[test]
fn block_tail_as_a_let_initializer() {
    let mut c = block_tail_let(1);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(2), CoroutineState::Yielded(3));
    assert_eq!(c.resume(4), CoroutineState::Complete(8)); // 1+2+4, +1
}

#[diapause::coroutine(yield = u32, resume = u32)]
fn if_else_tail(n: u32) -> u32 {
    let g: inner_sum::State = inner_sum(n);
    if n == 0 { 0 } else { yield_all!(g) }
}

#[test]
fn if_else_tails_as_the_function_tail() {
    let mut c = if_else_tail(100);
    assert_eq!(c.start(), CoroutineState::Yielded(100));
    assert_eq!(c.resume(1), CoroutineState::Yielded(101));
    assert_eq!(c.resume(2), CoroutineState::Complete(103));

    let mut c = if_else_tail(0);
    assert_eq!(c.start(), CoroutineState::Complete(0));
}

/// The context distributes recursively: an arm tail that is itself a
/// block whose tail delegates.
#[diapause::coroutine(yield = u32, resume = u32)]
fn nested_arm_block(n: u32) -> u32 {
    let g: inner_sum::State = inner_sum(n);
    match n {
        0 => 0,
        _ => {
            let h: u32 = n;
            {
                let _ = h;
                yield_all!(g)
            }
        }
    }
}

#[test]
fn nested_block_tail_inside_an_arm() {
    let mut c = nested_arm_block(100);
    assert_eq!(c.start(), CoroutineState::Yielded(100));
    assert_eq!(c.resume(1), CoroutineState::Yielded(101));
    assert_eq!(c.resume(2), CoroutineState::Complete(103));
}

// === `?` at expression tails ===

#[diapause::coroutine(yield = u32, resume = u32)]
fn arm_tail_try(n: u32) -> Result<u32, u32> {
    let g: checked_sum::State = checked_sum(n);
    let v: u32 = match n {
        0 => 0,
        _ => yield_all!(g)?,
    };
    Ok(v + 1)
}

#[test]
fn try_at_an_unbraced_arm_tail() {
    let mut c = arm_tail_try(10);
    assert_eq!(c.start(), CoroutineState::Yielded(10));
    assert_eq!(c.resume(5), CoroutineState::Complete(Ok(16))); // 10+5, +1

    // The Err completion exits the outer coroutine early.
    let mut c = arm_tail_try(10);
    assert_eq!(c.start(), CoroutineState::Yielded(10));
    assert_eq!(c.resume(0), CoroutineState::Complete(Err(10)));
}

/// The braced form of the same arm (kept multi-statement so rustfmt
/// does not collapse the braces away).
#[diapause::coroutine(yield = u32, resume = u32)]
fn braced_arm_tail_try(n: u32) -> Result<u32, u32> {
    let v: u32 = match n {
        0 => 0,
        _ => {
            let g: checked_sum::State = checked_sum(n);
            yield_all!(g)?
        }
    };
    Ok(v + 1)
}

#[test]
fn try_at_a_braced_arm_tail() {
    let mut c = braced_arm_tail_try(10);
    assert_eq!(c.start(), CoroutineState::Yielded(10));
    assert_eq!(c.resume(5), CoroutineState::Complete(Ok(16)));

    let mut c = braced_arm_tail_try(10);
    assert_eq!(c.start(), CoroutineState::Yielded(10));
    assert_eq!(c.resume(0), CoroutineState::Complete(Err(10)));
}

#[diapause::coroutine(yield = u32, resume = u32)]
fn block_tail_try(n: u32) -> Result<u32, u32> {
    let v: u32 = {
        let g: checked_sum::State = checked_sum(n);
        yield_all!(g)?
    };
    Ok(v + 1)
}

#[test]
fn try_at_a_block_tail() {
    let mut c = block_tail_try(10);
    assert_eq!(c.start(), CoroutineState::Yielded(10));
    assert_eq!(c.resume(5), CoroutineState::Complete(Ok(16)));

    let mut c = block_tail_try(10);
    assert_eq!(c.start(), CoroutineState::Yielded(10));
    assert_eq!(c.resume(0), CoroutineState::Complete(Err(10)));
}

// === `box` modifier and `yield_all_resume!` at expression tails ===

#[diapause::coroutine(yield = u32, resume = u32)]
fn boxed_arm_tail(n: u32) -> u32 {
    let sub: inner_sum::State = inner_sum(n);
    match n {
        0 => 0,
        _ => yield_all!(box sub),
    }
}

#[test]
fn boxed_delegation_at_an_arm_tail() {
    let mut c = boxed_arm_tail(100);
    assert_eq!(c.start(), CoroutineState::Yielded(100));
    assert_eq!(c.resume(1), CoroutineState::Yielded(101));
    assert_eq!(c.resume(2), CoroutineState::Complete(103));
}

#[diapause::coroutine(yield = u32, resume = u32)]
fn resume_if_else_tail(sub: inner_sum::State, rv: u32, flag: bool) -> u32 {
    if flag { yield_all_resume!(sub, rv) } else { 0 }
}

#[test]
fn resume_delegation_at_an_else_tail() {
    let mut sub = inner_sum(100);
    assert_eq!(sub.start(), CoroutineState::Yielded(100));
    let mut c = resume_if_else_tail(sub, 1, true);
    assert_eq!(c.start(), CoroutineState::Yielded(101));
    assert_eq!(c.resume(2), CoroutineState::Complete(103));
}

#[diapause::coroutine(yield = u32, resume = u32)]
fn resume_arm_tail_try(sub: checked_sum::State, rv: u32) -> Result<u32, u32> {
    let v: u32 = match rv {
        0 => 0,
        _ => yield_all_resume!(sub, rv)?,
    };
    Ok(v + 1)
}

#[test]
fn resume_delegation_with_try_at_an_arm_tail() {
    let mut sub = checked_sum(10);
    assert_eq!(sub.start(), CoroutineState::Yielded(10));
    let mut c = resume_arm_tail_try(sub, 5);
    assert_eq!(c.start(), CoroutineState::Complete(Ok(16))); // 10+5, +1
}
