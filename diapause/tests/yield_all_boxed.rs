//! Boxed delegation: `yield_all!(box sub)` / `yield_all_resume!(box sub, rv)`
//! store the delegated coroutine boxed in the state, which is what makes
//! recursive delegation representable (an unboxed recursive state would
//! be infinitely sized). Boxing is lazy: the entry transition runs on
//! the unboxed coroutine and only the suspending path allocates (see
//! `yield_all_boxed_no_alloc.rs` for the allocation-count proof).

use diapause::{Coroutine, CoroutineState};

/// The motivating case: a coroutine delegating to itself.
#[diapause::coroutine(yield = u32, resume = u32)]
fn countdown(n: u32) -> u32 {
    yield_!(n);
    if n == 0 {
        0
    } else {
        let sub: countdown::State = countdown(n - 1);
        let v: u32 = yield_all!(box sub);
        v + 1
    }
}

#[test]
fn recursive_delegation_through_a_boxed_state() {
    let mut c = countdown(2);
    assert_eq!(c.start(), CoroutineState::Yielded(2));
    assert_eq!(c.resume(0), CoroutineState::Yielded(1));
    assert_eq!(c.resume(0), CoroutineState::Yielded(0));
    assert_eq!(c.resume(0), CoroutineState::Complete(2));
}

#[diapause::coroutine(yield = u32, resume = u32)]
fn inner_sum(start: u32) -> u32 {
    let a = yield_!(start);
    let b = yield_!(start + a);
    start + a + b
}

/// A boxed delegation behaves exactly like an unboxed one; only the
/// storage differs.
#[diapause::coroutine(yield = u32, resume = u32)]
fn boxed_tail(start: u32) -> u32 {
    let sub: inner_sum::State = inner_sum(start);
    yield_all!(box sub)
}

#[test]
fn boxed_delegation_forwards_like_the_unboxed_form() {
    let mut c = boxed_tail(100);
    assert_eq!(c.start(), CoroutineState::Yielded(100));
    assert_eq!(c.resume(1), CoroutineState::Yielded(101));
    assert_eq!(c.resume(2), CoroutineState::Complete(103));
}

#[diapause::coroutine(yield = u32)]
fn count_to(n: u32) {
    for i in 0u32..n {
        yield_!(i);
    }
}

#[diapause::coroutine(yield = u32)]
fn boxed_statement_position(n: u32) -> u32 {
    let sub: count_to::State = count_to(n);
    yield_all!(box sub);
    9
}

#[test]
fn boxed_statement_position_discards_the_completion_value() {
    let mut c = boxed_statement_position(2);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(9));
}

#[diapause::coroutine(yield = u32, resume = u32)]
fn boxed_resume(sub: inner_sum::State, first: u32) -> u32 {
    let total: u32 = yield_all_resume!(box sub, first);
    yield_!(total);
    total + 1
}

#[test]
fn boxed_delegation_to_a_started_coroutine() {
    let mut sub = inner_sum(100);
    assert_eq!(sub.start(), CoroutineState::Yielded(100));
    let mut c = boxed_resume(sub, 1);
    assert_eq!(c.start(), CoroutineState::Yielded(101));
    assert_eq!(c.resume(2), CoroutineState::Yielded(103));
    assert_eq!(c.resume(0), CoroutineState::Complete(104));
}

/// The boxed state composes with `Clone` and serde like the unboxed
/// one: a coroutine suspended inside a boxed delegation round-trips
/// with the nested state included.
#[diapause::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn persistable(n: u32) -> u32 {
    let a = yield_!(n);
    n + a
}

#[diapause::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn persistable_outer(n: u32) -> u32 {
    let sub: persistable::State = persistable(n);
    yield_all!(box sub)
}

#[test]
fn boxed_delegation_state_round_trips_through_serde() {
    let mut c = persistable_outer(10);
    // Suspend inside the delegation, so the boxed state is live.
    assert_eq!(c.start(), CoroutineState::Yielded(10));
    let json = serde_json::to_string(&c).unwrap();
    let mut restored: persistable_outer::State = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.resume(5), CoroutineState::Complete(15));
    // The original is untouched and can be driven independently.
    assert_eq!(c.clone().resume(7), CoroutineState::Complete(17));
}
