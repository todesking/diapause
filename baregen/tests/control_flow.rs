//! Yields inside control flow (the v2 CFG pipeline): branches, loops,
//! break/continue, nesting, and early return.

// `let ... else` with a yield in the diverging block is a shape under
// test; the macro-expanded state machine trips the lint on it.
#![allow(clippy::diverging_sub_expression)]

use baregen::{Coroutine, CoroutineState};

#[baregen::coroutine(yield = u32, resume = u32)]
fn add_if(c: bool) -> u32 {
    let mut acc: u32 = 1;
    if c {
        let r = yield_!(acc);
        acc += r;
    }
    acc * 10
}

#[test]
fn yield_in_if_taken() {
    let mut c = add_if(true);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(5), CoroutineState::Complete(60));
}

#[test]
fn yield_in_if_skipped() {
    let mut c = add_if(false);
    assert_eq!(c.start(), CoroutineState::Complete(10));
}

/// The two branches keep different variables alive across their yields.
#[baregen::coroutine(yield = u32)]
fn branch_liveness(c: bool) -> u64 {
    let mut out: u64 = 0;
    if c {
        let a: u32 = 3;
        yield_!(1);
        out += a as u64;
    } else {
        let b2: u64 = 40;
        yield_!(2);
        out += b2;
    }
    out
}

#[test]
fn branches_with_differing_live_sets() {
    let mut c = branch_liveness(true);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(3));

    let mut c = branch_liveness(false);
    assert_eq!(c.start(), CoroutineState::Yielded(2));
    assert_eq!(c.resume(()), CoroutineState::Complete(40));
}

#[baregen::coroutine(yield = u32, resume = u32)]
fn match_yield(x: u32) -> u32 {
    let mut out: u32 = 0;
    match x {
        0 => {
            let r = yield_!(10);
            out += r;
        }
        _ => {
            let r = yield_!(20);
            out += r * 2;
        }
    }
    out
}

#[test]
fn yield_in_match_arms() {
    let mut c = match_yield(0);
    assert_eq!(c.start(), CoroutineState::Yielded(10));
    assert_eq!(c.resume(7), CoroutineState::Complete(7));

    let mut c = match_yield(9);
    assert_eq!(c.start(), CoroutineState::Yielded(20));
    assert_eq!(c.resume(7), CoroutineState::Complete(14));
}

/// `if let` with yields in both arms. The arm binding `v` is consumed
/// before the yield, so it never enters the state.
#[baregen::coroutine(yield = u32, resume = u32)]
fn if_let_yield(opt: Option<u32>) -> u32 {
    let mut out: u32 = 0;
    if let Some(v) = opt {
        out += v;
        let r = yield_!(out);
        out += r;
    } else {
        yield_!(0);
        out = 100;
    }
    out
}

#[test]
fn yield_in_if_let_matched() {
    let mut c = if_let_yield(Some(3));
    assert_eq!(c.start(), CoroutineState::Yielded(3));
    assert_eq!(c.resume(4), CoroutineState::Complete(7));
}

#[test]
fn yield_in_if_let_unmatched() {
    let mut c = if_let_yield(None);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(9), CoroutineState::Complete(100));
}

#[baregen::coroutine(yield = u32)]
fn else_if_let(a: bool, opt: Option<u32>) -> u32 {
    let mut out: u32 = 0;
    if a {
        yield_!(1);
        out = 1;
    } else if let Some(v) = opt {
        out = v;
        yield_!(2);
    } else {
        yield_!(3);
        out = 30;
    }
    out
}

#[test]
fn yield_in_else_if_let_chain() {
    let mut c = else_if_let(true, None);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(1));

    let mut c = else_if_let(false, Some(7));
    assert_eq!(c.start(), CoroutineState::Yielded(2));
    assert_eq!(c.resume(()), CoroutineState::Complete(7));

    let mut c = else_if_let(false, None);
    assert_eq!(c.start(), CoroutineState::Yielded(3));
    assert_eq!(c.resume(()), CoroutineState::Complete(30));
}

/// `if let` as a `let` initializer: the arm binding is rebound with an
/// annotation so it can cross the yield.
#[baregen::coroutine(yield = u32)]
fn if_let_value(opt: Option<u32>) -> u32 {
    let x: u32 = if let Some(v) = opt {
        let v2: u32 = v;
        yield_!(v2);
        v2 * 2
    } else {
        yield_!(0);
        7
    };
    x + 1
}

#[test]
fn if_let_in_value_position() {
    let mut c = if_let_value(Some(5));
    assert_eq!(c.start(), CoroutineState::Yielded(5));
    assert_eq!(c.resume(()), CoroutineState::Complete(11));

    let mut c = if_let_value(None);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Complete(8));
}

/// `let ... else` with a yield inside the diverging block. The pattern
/// binding is rebound with an annotation so it can cross the later
/// yield.
#[baregen::coroutine(yield = u32, resume = u32)]
fn unwrap_or_bail(opt: Option<u32>) -> u32 {
    let Some(v) = opt else {
        yield_!(404);
        return 0;
    };
    let v2: u32 = v;
    let r = yield_!(v2);
    v2 + r
}

#[test]
fn let_else_matched_continues() {
    let mut c = unwrap_or_bail(Some(5));
    assert_eq!(c.start(), CoroutineState::Yielded(5));
    assert_eq!(c.resume(3), CoroutineState::Complete(8));
}

#[test]
fn let_else_unmatched_diverges() {
    let mut c = unwrap_or_bail(None);
    assert_eq!(c.start(), CoroutineState::Yielded(404));
    assert_eq!(c.resume(9), CoroutineState::Complete(0));
}

/// `let ... else` diverging via `break` out of a yielding loop.
#[baregen::coroutine(yield = u32, resume = u32)]
fn sum_messages() -> u32 {
    let mut sum: u32 = 0;
    loop {
        let msg = yield_!(sum);
        let Some(v) = msg.checked_sub(1) else {
            yield_!(999);
            break;
        };
        sum += v;
    }
    sum
}

#[test]
fn let_else_break_exits_the_loop() {
    let mut c = sum_messages();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(5), CoroutineState::Yielded(4));
    assert_eq!(c.resume(3), CoroutineState::Yielded(6));
    assert_eq!(c.resume(0), CoroutineState::Yielded(999));
    assert_eq!(c.resume(0), CoroutineState::Complete(6));
}

// The design document's `totals` example.
#[baregen::coroutine(yield = u32, resume = u32)]
fn totals(n: u32) -> u32 {
    let mut sum: u32 = 0;
    let mut i: u32 = 0;
    while i < n {
        let r = yield_!(sum);
        sum += r;
        i += 1;
    }
    sum
}

#[test]
fn yield_in_while() {
    let mut c = totals(2);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(5), CoroutineState::Yielded(5));
    assert_eq!(c.resume(7), CoroutineState::Complete(12));
}

#[test]
fn while_condition_false_from_the_start() {
    let mut c = totals(0);
    assert_eq!(c.start(), CoroutineState::Complete(0));
}

#[baregen::coroutine(yield = u32, resume = bool)]
fn until_stop() -> u32 {
    let mut count: u32 = 0;
    loop {
        let stop = yield_!(count);
        if stop {
            yield_!(999);
            break;
        }
        count += 1;
    }
    count
}

/// The `if !go { .. break; }` statement contains no yield, so it stays
/// opaque and its `break` becomes a jump marker; the `assert!` next to
/// it is a foreign macro the marker replacement must leave alone.
#[baregen::coroutine(yield = u32, resume = bool)]
fn until_stop_checked() -> u32 {
    let mut count: u32 = 0;
    loop {
        let go = yield_!(count);
        if !go {
            assert!(count < 10, "runaway loop");
            break;
        }
        count += 1;
    }
    count
}

#[test]
fn foreign_macro_beside_an_opaque_break() {
    let mut c = until_stop_checked();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(true), CoroutineState::Yielded(1));
    assert_eq!(c.resume(false), CoroutineState::Complete(1));
}

#[test]
fn loop_with_break_in_nested_if() {
    let mut c = until_stop();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(false), CoroutineState::Yielded(1));
    assert_eq!(c.resume(true), CoroutineState::Yielded(999));
    assert_eq!(c.resume(false), CoroutineState::Complete(1));
}

#[baregen::coroutine(yield = u32, resume = bool)]
fn skipper() -> u32 {
    let mut i: u32 = 0;
    loop {
        i += 1;
        let again = yield_!(i);
        if again {
            yield_!(0);
            continue;
        }
        break;
    }
    i
}

#[test]
fn continue_restarts_the_loop() {
    let mut c = skipper();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(true), CoroutineState::Yielded(0));
    assert_eq!(c.resume(false), CoroutineState::Yielded(2));
    assert_eq!(c.resume(false), CoroutineState::Complete(2));
}

#[baregen::coroutine(yield = u32)]
fn nested_loops() -> u32 {
    let mut n: u32 = 0;
    'outer: loop {
        loop {
            yield_!(n);
            n += 1;
            if n >= 3 {
                yield_!(100);
                break 'outer;
            }
        }
    }
    n
}

#[test]
fn labeled_break_from_nested_loops() {
    let mut c = nested_loops();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Yielded(2));
    assert_eq!(c.resume(()), CoroutineState::Yielded(100));
    assert_eq!(c.resume(()), CoroutineState::Complete(3));
}

#[baregen::coroutine(yield = u32, resume = u32)]
fn early(limit: u32) -> u32 {
    let r = yield_!(limit);
    if r > limit {
        return 0;
    }
    let r2 = yield_!(r);
    r + r2
}

#[test]
fn early_return_completes() {
    let mut c = early(10);
    assert_eq!(c.start(), CoroutineState::Yielded(10));
    assert_eq!(c.resume(20), CoroutineState::Complete(0));
}

#[test]
fn early_return_not_taken() {
    let mut c = early(10);
    assert_eq!(c.start(), CoroutineState::Yielded(10));
    assert_eq!(c.resume(3), CoroutineState::Yielded(3));
    assert_eq!(c.resume(4), CoroutineState::Complete(7));
}

#[baregen::coroutine(yield = u32, resume = u32)]
fn early_in_loop() -> u32 {
    let mut total: u32 = 0;
    loop {
        let r = yield_!(total);
        if r == 0 {
            return total * 2;
        }
        total += r;
    }
}

#[test]
fn early_return_from_inside_a_loop() {
    let mut c = early_in_loop();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(5), CoroutineState::Yielded(5));
    assert_eq!(c.resume(0), CoroutineState::Complete(10));
}

/// A borrow taken inside the loop body and used after the yield is
/// re-established on every iteration.
#[baregen::coroutine(yield = i32, resume = i32)]
fn loop_borrow() -> i32 {
    let mut x: i32 = 0;
    let mut i: i32 = 0;
    while i < 2 {
        let p = &mut x;
        let r = yield_!(*p);
        *p += r;
        i += 1;
    }
    x
}

#[test]
fn borrow_across_loop_yield_is_rebuilt_each_iteration() {
    let mut c = loop_borrow();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(5), CoroutineState::Yielded(5));
    assert_eq!(c.resume(6), CoroutineState::Complete(11));
}

// === Jumps out of suspending loops from opaque statements ===
//
// A `break`/`continue` inside a statement without a yield_! (an "opaque"
// statement) still reaches its suspending target loop: lowering rewrites
// it into a transition that re-enters the dispatch loop.

/// The natural `if done { break; }` after a yield.
#[baregen::coroutine(yield = u32, resume = bool)]
fn stop_when_told() -> u32 {
    let mut i: u32 = 0;
    loop {
        let done = yield_!(i);
        if done {
            break;
        }
        i += 1;
    }
    i * 10
}

#[test]
fn opaque_break_exits_the_loop() {
    let mut c = stop_when_told();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(false), CoroutineState::Yielded(1));
    assert_eq!(c.resume(true), CoroutineState::Complete(10));
}

/// An opaque `continue` targeting a `while` loop re-evaluates its
/// condition at the header.
#[baregen::coroutine(yield = u32, resume = u32)]
fn sum_odd_replies(n: u32) -> u32 {
    let mut sum: u32 = 0;
    let mut i: u32 = 0;
    while i < n {
        i += 1;
        let r = yield_!(i);
        if r.is_multiple_of(2) {
            continue;
        }
        sum += r;
    }
    sum
}

#[test]
fn opaque_continue_restarts_the_while_loop() {
    let mut c = sum_odd_replies(3);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(2), CoroutineState::Yielded(2));
    assert_eq!(c.resume(3), CoroutineState::Yielded(3));
    assert_eq!(c.resume(5), CoroutineState::Complete(8));
}

/// An opaque `continue` targeting a `for` loop advances its stored
/// iterator.
#[baregen::coroutine(yield = u32)]
fn sum_even_indices(n: u32) -> u32 {
    let mut sum: u32 = 0;
    for x in 0u32..n {
        yield_!(x);
        if x % 2 == 1 {
            continue;
        }
        sum += x;
    }
    sum
}

#[test]
fn opaque_continue_advances_the_for_loop() {
    let mut c = sum_even_indices(4);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Yielded(2));
    assert_eq!(c.resume(()), CoroutineState::Yielded(3));
    assert_eq!(c.resume(()), CoroutineState::Complete(2));
}

/// A labeled break out of a whole opaque nested loop, with a variable
/// assigned inside the statement before the jump and live at the exit.
#[baregen::coroutine(yield = u32, resume = u32)]
fn find_seven() -> u32 {
    let mut found: u32 = 0;
    'outer: loop {
        let base = yield_!(found);
        for k in 0..10u32 {
            if base + k == 7 {
                found = base + k;
                break 'outer;
            }
        }
    }
    found
}

#[test]
fn opaque_labeled_break_from_nested_opaque_loop() {
    let mut c = find_seven();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(20), CoroutineState::Yielded(0));
    assert_eq!(c.resume(5), CoroutineState::Complete(7));
}

/// An opaque statement mixing jumps that stay local (the inner loop's
/// own `break`) with one that escapes to the suspending loop.
#[baregen::coroutine(yield = u32, resume = u32)]
fn smallest_root() -> u32 {
    let mut best: u32 = 0;
    'search: loop {
        let target = yield_!(best);
        let mut k: u32 = 0;
        loop {
            if k * k >= target {
                break;
            }
            k += 1;
        }
        if k == target {
            best = k;
            break 'search;
        }
    }
    best
}

#[test]
fn local_breaks_stay_inside_the_opaque_statement() {
    let mut c = smallest_root();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(3), CoroutineState::Yielded(0));
    assert_eq!(c.resume(1), CoroutineState::Complete(1));
}

/// Both jump kinds escaping from one opaque `match` statement.
#[baregen::coroutine(yield = u32, resume = u32)]
fn accumulate_until_zero() -> u32 {
    let mut acc: u32 = 0;
    loop {
        let r = yield_!(acc);
        match r {
            0 => break,
            1 => continue,
            n => acc += n,
        }
    }
    acc
}

#[test]
fn opaque_break_and_continue_in_match_arms() {
    let mut c = accumulate_until_zero();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(5), CoroutineState::Yielded(5));
    assert_eq!(c.resume(1), CoroutineState::Yielded(5));
    assert_eq!(c.resume(0), CoroutineState::Complete(5));
}

/// An opaque `break` targeting an expanded labeled block.
#[baregen::coroutine(yield = u32)]
fn skippable_block(c: bool) -> u32 {
    let mut out: u32 = 1;
    'b: {
        yield_!(out);
        if c {
            break 'b;
        }
        out += 10;
    }
    out * 2
}

#[test]
fn opaque_break_out_of_labeled_block() {
    let mut c = skippable_block(true);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(2));

    let mut c = skippable_block(false);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(22));
}

/// A valued opaque `break` out of a `let`-initializer loop assigns the
/// binding before jumping to the join.
#[baregen::coroutine(yield = u32, resume = u32)]
fn first_big_reply() -> u32 {
    let x: u32 = loop {
        let r = yield_!(1);
        if r > 3 {
            break r * 2;
        }
    };
    x + 1
}

#[test]
fn opaque_valued_break_into_let_initializer() {
    let mut c = first_big_reply();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(2), CoroutineState::Yielded(1));
    assert_eq!(c.resume(5), CoroutineState::Complete(11));
}

/// A valued opaque `break` out of a tail-position loop completes the
/// coroutine directly.
#[baregen::coroutine(yield = u32, resume = u32)]
fn reply_after_nine() -> u32 {
    loop {
        let r = yield_!(0);
        if r == 9 {
            break r + 1;
        }
    }
}

#[test]
fn opaque_valued_break_out_of_tail_loop() {
    let mut c = reply_after_nine();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(3), CoroutineState::Yielded(0));
    assert_eq!(c.resume(9), CoroutineState::Complete(10));
}
