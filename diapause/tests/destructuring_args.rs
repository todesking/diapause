//! Destructuring patterns in coroutine arguments: the value is stored
//! in the state under a fresh `__argN` name and destructured by a
//! synthesized `let` at the top of the body.

use diapause::{Coroutine, CoroutineState};

#[diapause::coroutine(yield = u32)]
fn sum_pair((a, b): (u32, u32)) -> u32 {
    let sum: u32 = a + b;
    yield_!(sum);
    sum * 2
}

#[test]
fn tuple_pattern_arg() {
    let mut c = sum_pair((3, 4));
    assert_eq!(c.start(), CoroutineState::Yielded(7));
    assert_eq!(c.resume(()), CoroutineState::Complete(14));
}

#[diapause::coroutine(yield = u32)]
fn accumulate((mut acc, step): (u32, u32)) -> u32 {
    acc += step;
    let total: u32 = acc;
    yield_!(total);
    total
}

#[test]
fn mut_pattern_component() {
    let mut c = accumulate((10, 5));
    assert_eq!(c.start(), CoroutineState::Yielded(15));
    assert_eq!(c.resume(()), CoroutineState::Complete(15));
}

struct Point {
    x: u32,
    y: u32,
}

#[diapause::coroutine(yield = u32)]
fn manhattan(Point { x, y }: Point) -> u32 {
    let d: u32 = x + y;
    yield_!(d);
    d
}

#[test]
fn struct_pattern_arg() {
    let mut c = manhattan(Point { x: 3, y: 4 });
    assert_eq!(c.start(), CoroutineState::Yielded(7));
    assert_eq!(c.resume(()), CoroutineState::Complete(7));
}

#[diapause::coroutine(yield = u32)]
fn ignores_first(_: u32, n: u32) -> u32 {
    yield_!(n);
    n + 1
}

#[test]
fn wildcard_arg() {
    let mut c = ignores_first(99, 5);
    assert_eq!(c.start(), CoroutineState::Yielded(5));
    assert_eq!(c.resume(()), CoroutineState::Complete(6));
}

#[diapause::coroutine(yield = u32)]
fn deref_pair(&(a, b): &(u32, u32)) -> u32 {
    let sum: u32 = a + b;
    yield_!(sum);
    sum
}

#[test]
fn reference_pattern_arg() {
    let pair = (1, 2);
    let mut c = deref_pair(&pair);
    assert_eq!(c.start(), CoroutineState::Yielded(3));
    assert_eq!(c.resume(()), CoroutineState::Complete(3));
}

// A destructured component crossing a yield needs an annotated rebind
// (as the compile error suggests); shadowing with the same name works.
#[diapause::coroutine(yield = u32)]
fn rebound((a, b): (u32, u32)) -> u32 {
    let a: u32 = a;
    let b: u32 = b;
    yield_!(a);
    a + b
}

#[test]
fn rebound_components_cross_yields() {
    let mut c = rebound((3, 4));
    assert_eq!(c.start(), CoroutineState::Yielded(3));
    assert_eq!(c.resume(()), CoroutineState::Complete(7));
}
