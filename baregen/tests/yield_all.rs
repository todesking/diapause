//! `yield_all!` delegation: forwarding inner yields and resume values,
//! and completing with the inner coroutine's return value.

use baregen::{Coroutine, CoroutineState};

#[baregen::coroutine(yield = u32, resume = u32)]
fn inner_sum(start: u32) -> u32 {
    let a = yield_!(start);
    let b = yield_!(start + a);
    start + a + b
}

#[baregen::coroutine(yield = u32, resume = u32)]
fn outer(n: u32) -> u32 {
    let before = yield_!(n);
    let g: inner_sum::State = inner_sum(before);
    let sub: u32 = yield_all!(g);
    yield_!(sub);
    sub + n
}

#[test]
fn delegation_forwards_yields_and_resume_values() {
    let mut c = outer(1);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    // `10` becomes the inner coroutine's start argument.
    assert_eq!(c.resume(10), CoroutineState::Yielded(10)); // inner yields start
    // Resume values pass through to the suspended inner coroutine.
    assert_eq!(c.resume(2), CoroutineState::Yielded(12)); // inner yields start + a
    assert_eq!(c.resume(3), CoroutineState::Yielded(15)); // inner completes with 10+2+3
    assert_eq!(c.resume(0), CoroutineState::Complete(16)); // sub + n
}

#[baregen::coroutine(yield = u32)]
#[derive(Clone)]
fn count_to(n: u32) {
    for i in 0u32..n {
        yield_!(i);
    }
}

#[baregen::coroutine(yield = u32)]
fn statement_position(n: u32) -> u32 {
    let g: count_to::State = count_to(n);
    yield_all!(g);
    99
}

#[test]
fn statement_position_discards_the_completion_value() {
    let mut c = statement_position(2);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(99));
}

#[baregen::coroutine(yield = u32, resume = u32)]
fn tail_position(start: u32) -> u32 {
    let g: inner_sum::State = inner_sum(start);
    yield_all!(g)
}

#[test]
fn tail_position_returns_the_completion_value() {
    let mut c = tail_position(100);
    assert_eq!(c.start(), CoroutineState::Yielded(100));
    assert_eq!(c.resume(1), CoroutineState::Yielded(101));
    assert_eq!(c.resume(2), CoroutineState::Complete(103));
}

/// A brace-delimited invocation is the one trailing-macro form that
/// reaches lowering as `Stmt::Macro` without a semicolon (syn parses
/// trailing paren/bracket macros as trailing expressions).
#[baregen::coroutine(yield = u32, resume = u32)]
fn tail_position_brace(start: u32) -> u32 {
    let g: inner_sum::State = inner_sum(start);
    yield_all! { g }
}

#[test]
fn brace_delimited_tail_delegation_returns_the_completion_value() {
    let mut c = tail_position_brace(100);
    assert_eq!(c.start(), CoroutineState::Yielded(100));
    assert_eq!(c.resume(1), CoroutineState::Yielded(101));
    assert_eq!(c.resume(2), CoroutineState::Complete(103));
}

#[baregen::coroutine(yield = u32)]
fn completes_immediately() -> u32 {
    7
}

#[baregen::coroutine(yield = u32)]
fn delegate_to_non_yielding() -> u32 {
    let g: completes_immediately::State = completes_immediately();
    let v: u32 = yield_all!(g);
    v + 1
}

#[test]
fn inner_completing_without_yielding_never_suspends_the_outer() {
    let mut c = delegate_to_non_yielding();
    assert_eq!(c.start(), CoroutineState::Complete(8));
}

#[baregen::coroutine(yield = u32)]
fn sequential(n: u32) -> u32 {
    let g1: count_to::State = count_to(n);
    yield_all!(g1);
    let g2: count_to::State = count_to(n);
    yield_all!(g2);
    n
}

#[test]
fn two_sequential_delegations_get_distinct_synthetic_names() {
    let mut c = sequential(2);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(2));
}

// The inner coroutine's state moves into the outer state by value, so
// derives compose across the nesting.
#[baregen::coroutine(yield = u32)]
#[derive(Clone)]
fn cloneable_outer(n: u32) -> u32 {
    let g: count_to::State = count_to(n);
    yield_all!(g);
    n
}

#[test]
fn suspended_delegation_can_be_cloned() {
    // count_to's State must be Clone for the outer derive to compile.
    let mut c = cloneable_outer(2);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    let mut snapshot = c.clone();
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(snapshot.resume(()), CoroutineState::Yielded(1));
}

#[baregen::coroutine(yield = u32)]
fn delegate_inside_loop(n: u32) -> u32 {
    let mut total: u32 = 0;
    for _ in 0u32..2 {
        let g: count_to::State = count_to(n);
        yield_all!(g);
        total += n;
    }
    total
}

#[test]
fn delegation_works_inside_an_expanded_loop() {
    let mut c = delegate_inside_loop(2);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(4));
}

#[test]
#[should_panic(expected = "Already started")]
fn delegating_to_a_started_coroutine_panics() {
    let mut g = inner_sum(1);
    g.start();
    // Rebuild the outer coroutine around the already-started state.
    #[baregen::coroutine(yield = u32, resume = u32)]
    fn resume_started(g: inner_sum::State) -> u32 {
        let v: u32 = yield_all!(g);
        v
    }
    let mut c = resume_started(g);
    c.start();
}
