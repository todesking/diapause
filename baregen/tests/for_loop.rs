//! Yields inside `for` loops: the iterator is stored in the state with
//! its concrete `<T as IntoIterator>::IntoIter` type.

use baregen::{Coroutine, CoroutineState};

#[baregen::coroutine(yield = u32)]
fn count(n: u32) -> u32 {
    let mut sum: u32 = 0;
    for i in 0u32..n {
        yield_!(i);
        sum += i;
    }
    sum
}

#[test]
fn for_over_a_range() {
    let mut c = count(3);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Yielded(2));
    assert_eq!(c.resume(()), CoroutineState::Complete(3));
}

#[test]
fn for_over_an_empty_range() {
    let mut c = count(0);
    assert_eq!(c.start(), CoroutineState::Complete(0));
}

#[baregen::coroutine(yield = u32, resume = u32)]
fn weighted(n: u32) -> u32 {
    let mut acc: u32 = 0;
    for i in 1u32..=n {
        let w = yield_!(i);
        acc += i * w;
    }
    acc
}

#[test]
fn resume_values_feed_the_loop_body() {
    let mut c = weighted(2);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(10), CoroutineState::Yielded(2));
    assert_eq!(c.resume(100), CoroutineState::Complete(210));
}

/// Arrays iterate by value with a concrete `array::IntoIter` state.
#[baregen::coroutine(yield = u32)]
fn drain() -> u32 {
    let items: [u32; 3] = [7, 8, 9];
    let mut sum: u32 = 0;
    for x in items {
        yield_!(x);
        sum += x;
    }
    sum
}

#[test]
fn for_over_an_annotated_array() {
    let mut c = drain();
    assert_eq!(c.start(), CoroutineState::Yielded(7));
    assert_eq!(c.resume(()), CoroutineState::Yielded(8));
    assert_eq!(c.resume(()), CoroutineState::Yielded(9));
    assert_eq!(c.resume(()), CoroutineState::Complete(24));
}

#[baregen::coroutine(yield = u32)]
fn ticks(n: u32) -> u32 {
    let mut t: u32 = 0;
    for _ in 0u32..n {
        yield_!(t);
        t += 1;
    }
    t
}

#[test]
fn wildcard_loop_variable() {
    let mut c = ticks(2);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(2));
}

#[baregen::coroutine(yield = u32)]
fn doubler(n: u32) -> u32 {
    let mut sum: u32 = 0;
    for mut i in 0u32..n {
        i *= 2;
        yield_!(i);
        sum += i;
    }
    sum
}

#[test]
fn mut_loop_variable_crosses_the_yield() {
    let mut c = doubler(2);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(2));
    assert_eq!(c.resume(()), CoroutineState::Complete(2));
}

#[baregen::coroutine(yield = u32)]
fn grid(w: u32, h: u32) -> u32 {
    let mut cells: u32 = 0;
    for y in 0u32..h {
        for x in 0u32..w {
            yield_!(y * 10 + x);
            cells += 1;
        }
    }
    cells
}

#[test]
fn for_nested_in_for() {
    let mut c = grid(2, 2);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Yielded(10));
    assert_eq!(c.resume(()), CoroutineState::Yielded(11));
    assert_eq!(c.resume(()), CoroutineState::Complete(4));
}

#[baregen::coroutine(yield = u32)]
fn maybe_iterate(go: bool, n: u32) -> u32 {
    let mut seen: u32 = 0;
    if go {
        for i in 0u32..n {
            yield_!(i);
            seen += 1;
        }
    }
    seen
}

#[test]
fn for_nested_in_if() {
    let mut c = maybe_iterate(true, 2);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(2));

    let mut c = maybe_iterate(false, 2);
    assert_eq!(c.start(), CoroutineState::Complete(0));
}

#[baregen::coroutine(yield = u32, resume = u32)]
fn pick() -> u32 {
    let mut sum: u32 = 0;
    for i in 0u32..100 {
        let cmd = yield_!(i);
        if cmd == 1 {
            yield_!(1000 + i);
            continue;
        }
        if cmd == 2 {
            yield_!(2000 + i);
            break;
        }
        sum += i;
    }
    sum
}

#[test]
fn break_and_continue_inside_a_for() {
    let mut c = pick();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    // cmd 0: accumulate.
    assert_eq!(c.resume(0), CoroutineState::Yielded(1));
    // cmd 1: skip without accumulating, next iteration continues.
    assert_eq!(c.resume(1), CoroutineState::Yielded(1001));
    assert_eq!(c.resume(0), CoroutineState::Yielded(2));
    // cmd 2: leave the loop; only i == 0 was accumulated.
    assert_eq!(c.resume(2), CoroutineState::Yielded(2002));
    assert_eq!(c.resume(0), CoroutineState::Complete(0));
}

#[baregen::coroutine(yield = u32)]
fn diagonal(n: u32) -> u32 {
    let mut found: u32 = 0;
    'outer: for i in 0u32..n {
        for j in 0u32..n {
            yield_!(i * 10 + j);
            if i + j >= 2 {
                yield_!(999);
                break 'outer;
            }
            found += 1;
        }
    }
    found
}

#[test]
fn labeled_break_from_a_nested_for() {
    let mut c = diagonal(3);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Yielded(2));
    assert_eq!(c.resume(()), CoroutineState::Yielded(999));
    assert_eq!(c.resume(()), CoroutineState::Complete(2));
}

/// The head expression may borrow an argument: the reference points
/// outside the state, so storing the iterator is sound.
#[baregen::coroutine(yield = u32)]
fn total_of<'a>(xs: &'a [u32; 3]) -> u32 {
    let mut sum: u32 = 0;
    for x in xs {
        yield_!(*x);
        sum += *x;
    }
    sum
}

#[test]
fn for_over_a_borrowed_argument() {
    let data = [1, 2, 3];
    let mut c = total_of(&data);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Yielded(2));
    assert_eq!(c.resume(()), CoroutineState::Yielded(3));
    assert_eq!(c.resume(()), CoroutineState::Complete(6));
}
