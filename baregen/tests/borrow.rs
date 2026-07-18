//! Direct borrows crossing a yield are reconstructed after resume: the
//! borrow's source is stored in the state and the borrow is re-created.

use baregen::{Coroutine, CoroutineState};

struct Value {
    foo: i32,
}

#[baregen::coroutine(yield = i32)]
fn mut_borrow_across_yield() -> i32 {
    let mut x: Value = Value { foo: 1 };
    let y = &mut x;
    yield_!(123);
    y.foo = 99;
    x.foo
}

#[test]
fn write_through_reborrow_reaches_the_source() {
    let mut c = mut_borrow_across_yield();
    assert_eq!(c.start(), CoroutineState::Yielded(123));
    assert_eq!(c.resume(()), CoroutineState::Complete(99));
}

#[baregen::coroutine(yield = u32)]
fn shared_borrow_across_yield() -> usize {
    let s: String = String::from("hello");
    let r = &s;
    yield_!(1);
    r.len()
}

#[test]
fn shared_borrow_is_reconstructed() {
    let mut c = shared_borrow_across_yield();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(5));
}

#[baregen::coroutine(yield = i32)]
fn borrow_across_two_yields() -> i32 {
    let mut n: i32 = 0;
    let p = &mut n;
    yield_!(1);
    *p += 1;
    yield_!(2);
    *p += 2;
    n
}

#[test]
fn borrow_is_reconstructed_in_each_segment() {
    let mut c = borrow_across_two_yields();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Yielded(2));
    assert_eq!(c.resume(()), CoroutineState::Complete(3));
}

#[baregen::coroutine(yield = i32)]
fn borrow_used_before_and_after_yield() -> i32 {
    let mut n: i32 = 10;
    let p = &mut n;
    *p += 1;
    yield_!(1);
    *p += 1;
    n
}

#[test]
fn original_borrow_stays_when_used_before_yield() {
    let mut c = borrow_used_before_and_after_yield();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(12));
}

// The resume block after the first yield absorbs the match's join block
// during CFG simplification, shifting the borrow `let` to a later
// statement index. Dropping the borrow must not take `x += 10;` with it.
#[baregen::coroutine(yield = i32)]
fn borrow_let_in_merged_join_block() -> i32 {
    let mut x: i32 = 1;
    match () {
        _ => {
            yield_!(1);
            x += 10;
        }
    }
    let y = &mut x;
    yield_!(2);
    *y += 1;
    x
}

#[test]
fn statement_before_a_dropped_borrow_survives_block_merging() {
    let mut c = borrow_let_in_merged_join_block();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Yielded(2));
    assert_eq!(c.resume(()), CoroutineState::Complete(12));
}

// Shadowing a former borrow source is fine once the borrow is dead:
// only the new `x` crosses the second yield. (Shadowing it while the
// borrow is still live is a compile error; see
// tests/compile_fail/borrow_source_shadowed.rs.)
#[baregen::coroutine(yield = u32)]
fn sequential_shadowing_after_borrow_dies() -> u32 {
    let x: u32 = 1;
    let y = &x;
    yield_!(*y);
    let kept: u32 = *y + 1;
    let x: u32 = 40;
    yield_!(x);
    x + kept
}

#[test]
fn shadowing_a_dead_borrow_source_uses_the_new_binding() {
    let mut c = sequential_shadowing_after_borrow_dies();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Yielded(40));
    assert_eq!(c.resume(()), CoroutineState::Complete(42));
}

// The shadowing binding is itself a borrow whose `let` is dropped from
// the emitted arm (it is rebuilt in the next region), so the outer `y`
// is transferred untouched.
#[baregen::coroutine(yield = u32)]
fn shadowing_borrow_let_is_dropped() -> u32 {
    let y: u32 = 1;
    let p = &y;
    yield_!(0);
    let x: u32 = 2;
    let y = &x;
    yield_!(0);
    *p + *y
}

#[test]
fn dropped_shadowing_borrow_reads_both_sources() {
    let mut c = shadowing_borrow_let_is_dropped();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Complete(3));
}
