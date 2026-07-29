//! `yield_all_resume!` delegation to an already-started coroutine:
//! entering via `resume` with an explicit first resume value, then
//! forwarding yields and resume values like `yield_all!`.

use diapause::{Coroutine, CoroutineState};

#[diapause::coroutine(yield = u32, resume = u32)]
fn inner_sum(start: u32) -> u32 {
    let a = yield_!(start);
    let b = yield_!(start + a);
    start + a + b
}

#[diapause::coroutine(yield = u32, resume = u32)]
fn outer(sub: inner_sum::State, first: u32) -> u32 {
    let total: u32 = yield_all_resume!(sub, first);
    yield_!(total);
    total + 1
}

#[test]
fn resumes_a_started_coroutine_and_forwards_the_rest() {
    let mut sub = inner_sum(100);
    // The first yield is consumed here, before the delegation begins.
    assert_eq!(sub.start(), CoroutineState::Yielded(100));
    let mut c = outer(sub, 1); // `1` becomes `a` inside inner_sum
    assert_eq!(c.start(), CoroutineState::Yielded(101)); // inner yields start + a
    // The inner coroutine completes with 100+1+2, bound to `total`.
    assert_eq!(c.resume(2), CoroutineState::Yielded(103));
    assert_eq!(c.resume(0), CoroutineState::Complete(104));
}

#[diapause::coroutine(yield = u32)]
fn count_to(n: u32) {
    for i in 0u32..n {
        yield_!(i);
    }
}

#[diapause::coroutine(yield = u32)]
fn statement_position(sub: count_to::State) -> u32 {
    yield_all_resume!(sub, ());
    9
}

#[test]
fn statement_position_discards_the_completion_value() {
    let mut sub = count_to(3);
    assert_eq!(sub.start(), CoroutineState::Yielded(0));
    let mut c = statement_position(sub);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Yielded(2));
    assert_eq!(c.resume(()), CoroutineState::Complete(9));
}

/// The resume value may be an arbitrary yield-free expression; it is
/// consumed by the entry `resume` call and never stored in the state.
#[diapause::coroutine(yield = u32, resume = u32)]
fn tail_position(sub: inner_sum::State, n: u32) -> u32 {
    yield_all_resume!(sub, n + 1)
}

#[test]
fn tail_position_with_an_expression_resume_value() {
    let mut sub = inner_sum(10);
    assert_eq!(sub.start(), CoroutineState::Yielded(10));
    let mut c = tail_position(sub, 4); // resume value 4+1 becomes `a`
    assert_eq!(c.start(), CoroutineState::Yielded(15));
    assert_eq!(c.resume(3), CoroutineState::Complete(18)); // 10+5+3
}

/// Starting the sub-coroutine inside the body, forwarding its first
/// yield by hand, and delegating the rest.
#[diapause::coroutine(yield = u32, resume = u32)]
fn start_inside(n: u32) -> u32 {
    let mut sub: inner_sum::State = inner_sum(n);
    let first = match sub.start() {
        CoroutineState::Yielded(y) => y,
        CoroutineState::Complete(v) => return v,
    };
    let rv = yield_!(first);
    yield_all_resume!(sub, rv)
}

#[test]
fn starting_the_sub_coroutine_inline_then_delegating() {
    let mut c = start_inside(10);
    assert_eq!(c.start(), CoroutineState::Yielded(10));
    assert_eq!(c.resume(5), CoroutineState::Yielded(15));
    assert_eq!(c.resume(3), CoroutineState::Complete(18));
}

/// The delegated coroutine may complete on the entry `resume` call
/// without the outer coroutine suspending at all.
#[diapause::coroutine(yield = u32, resume = u32)]
fn completes_on_entry(sub: last_leg::State) -> u32 {
    let v: u32 = yield_all_resume!(sub, 7);
    v * 2
}

#[diapause::coroutine(yield = u32, resume = u32)]
fn last_leg(n: u32) -> u32 {
    let a = yield_!(n);
    n + a
}

#[test]
fn entry_resume_may_complete_immediately() {
    let mut sub = last_leg(1);
    assert_eq!(sub.start(), CoroutineState::Yielded(1));
    let mut c = completes_on_entry(sub);
    assert_eq!(c.start(), CoroutineState::Complete(16)); // (1+7)*2
}

#[test]
#[should_panic]
fn delegating_to_an_unstarted_coroutine_panics() {
    let sub = inner_sum(1); // never started
    let mut c = outer(sub, 0);
    let _ = c.start();
}
