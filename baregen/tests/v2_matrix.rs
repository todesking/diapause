//! Combination matrix for the v2 control-flow support: nesting between
//! if / match / loop / while / for mixed with break, continue, early
//! return, and `?`, plus poisoning inside control flow and regressions
//! for derive / generics / reference arguments.
//!
//! Single-construct behaviour lives in control_flow.rs, for_loop.rs, and
//! try_operator.rs; every coroutine here exercises a distinct structural
//! interaction between at least two constructs.

use baregen::{Coroutine, CoroutineState};

// === Nesting matrix ===

/// `while let` with a yield in the body. The arm binding `x` is consumed
/// before the yield, so it never enters the state.
#[baregen::coroutine(yield = u32, resume = u32)]
fn drain_stack() -> u32 {
    let mut stack: Vec<u32> = vec![3, 2, 1];
    let mut sum: u32 = 0;
    while let Some(x) = stack.pop() {
        let r = yield_!(x);
        sum += r;
    }
    sum
}

#[test]
fn while_let_with_yield_in_body() {
    let mut c = drain_stack();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(10), CoroutineState::Yielded(2));
    assert_eq!(c.resume(20), CoroutineState::Yielded(3));
    assert_eq!(c.resume(30), CoroutineState::Complete(60));
}

/// match inside loop: a command dispatcher with break in one arm and a
/// nested yield in another.
#[baregen::coroutine(yield = u32, resume = u32)]
fn dispatcher() -> u32 {
    let mut acc: u32 = 0;
    loop {
        let cmd = yield_!(acc);
        match cmd {
            0 => break,
            1 => {
                let v = yield_!(100);
                acc += v;
            }
            _ => acc += cmd,
        }
    }
    acc
}

#[test]
fn match_inside_loop_dispatches_commands() {
    let mut c = dispatcher();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    // cmd 5: fall-through arm accumulates the command itself.
    assert_eq!(c.resume(5), CoroutineState::Yielded(5));
    // cmd 1: nested yield, resume value accumulates.
    assert_eq!(c.resume(1), CoroutineState::Yielded(100));
    assert_eq!(c.resume(7), CoroutineState::Yielded(12));
    // cmd 0: break out of the loop.
    assert_eq!(c.resume(0), CoroutineState::Complete(12));
}

/// while inside a match arm; the other arm yields directly.
#[baregen::coroutine(yield = u32)]
fn shape(kind: u32) -> u32 {
    let mut n: u32 = 0;
    match kind {
        0 => {
            while n < 2 {
                yield_!(n);
                n += 1;
            }
        }
        _ => {
            yield_!(99);
            n += 10;
        }
    }
    n
}

#[test]
fn while_inside_match_arm() {
    let mut c = shape(0);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(2));

    let mut c = shape(1);
    assert_eq!(c.start(), CoroutineState::Yielded(99));
    assert_eq!(c.resume(()), CoroutineState::Complete(10));
}

/// while inside for: each item is retried until the resume value accepts
/// it, then the outer for advances. The exit condition lives in the
/// while header via a flag (a bare `if ok { break; }` would be a jump
/// from a yield-free statement, which is rejected).
#[baregen::coroutine(yield = u32, resume = bool)]
fn retry(n: u32) -> u32 {
    let mut done: u32 = 0;
    for i in 0u32..n {
        let mut ok: bool = false;
        while !ok {
            let r = yield_!(i);
            if r {
                ok = true;
            }
        }
        done += 1;
    }
    done
}

#[test]
fn while_inside_for_retries_each_item() {
    let mut c = retry(2);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(false), CoroutineState::Yielded(0));
    assert_eq!(c.resume(true), CoroutineState::Yielded(1));
    assert_eq!(c.resume(true), CoroutineState::Complete(2));
}

/// if/else inside for with yields in both branches and a continue in one.
#[baregen::coroutine(yield = u32)]
fn classify(n: u32) -> u32 {
    let mut odd_sum: u32 = 0;
    for i in 0u32..n {
        if i % 2 == 0 {
            yield_!(i * 10);
            continue;
        } else {
            yield_!(i);
            odd_sum += i;
        }
    }
    odd_sum
}

#[test]
fn if_else_inside_for_with_continue() {
    let mut c = classify(4);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Yielded(20));
    assert_eq!(c.resume(()), CoroutineState::Yielded(3));
    assert_eq!(c.resume(()), CoroutineState::Complete(4));
}

#[derive(Debug, PartialEq)]
pub struct ParseError;

fn parse(s: &str) -> Result<u32, ParseError> {
    s.parse().map_err(|_| ParseError)
}

/// `?` inside an expanded for body, after a yield: the failure path exits
/// mid-iteration with the iterator still stored in the state.
#[baregen::coroutine(yield = u32)]
fn parse_all(a: &'static str, b: &'static str) -> Result<u32, ParseError> {
    let items: [&'static str; 2] = [a, b];
    let mut sum: u32 = 0;
    for s in items {
        yield_!(sum);
        sum += parse(s)?;
    }
    Ok(sum)
}

#[test]
fn try_inside_for_after_yield() {
    let mut c = parse_all("1", "2");
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(Ok(3)));

    let mut c = parse_all("1", "x");
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(Err(ParseError)));
}

/// `?` mixed with break inside an expanded while. The break sits inside
/// an if that contains a yield of its own (a jump out of a
/// yield-containing loop must not come from a yield-free statement).
#[baregen::coroutine(yield = u32, resume = u32)]
fn budget(mut left: u32) -> Option<u32> {
    let mut spent: u32 = 0;
    while left > 0 {
        let cost = yield_!(left);
        if cost == 0 {
            yield_!(9999);
            break;
        }
        left = left.checked_sub(cost)?;
        spent += cost;
    }
    Some(spent)
}

#[test]
fn try_and_break_inside_while() {
    // Normal path with an explicit break.
    let mut c = budget(10);
    assert_eq!(c.start(), CoroutineState::Yielded(10));
    assert_eq!(c.resume(3), CoroutineState::Yielded(7));
    assert_eq!(c.resume(0), CoroutineState::Yielded(9999));
    assert_eq!(c.resume(0), CoroutineState::Complete(Some(3)));

    // Overspend: checked_sub returns None and `?` short-circuits.
    let mut c = budget(10);
    assert_eq!(c.start(), CoroutineState::Yielded(10));
    assert_eq!(c.resume(11), CoroutineState::Complete(None));

    // Exact spend: the while condition ends the loop.
    let mut c = budget(4);
    assert_eq!(c.start(), CoroutineState::Yielded(4));
    assert_eq!(c.resume(4), CoroutineState::Complete(Some(4)));
}

/// Early return, continue, and a nested yield in different arms of a
/// match inside a for.
#[baregen::coroutine(yield = u32, resume = u32)]
fn find() -> u32 {
    for i in 0u32..10 {
        let cmd = yield_!(i);
        match cmd {
            1 => return i * 100,
            2 => {
                yield_!(1000 + i);
            }
            _ => continue,
        }
    }
    0
}

#[test]
fn return_continue_and_yield_in_match_arms_inside_for() {
    let mut c = find();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    // cmd 9: continue arm.
    assert_eq!(c.resume(9), CoroutineState::Yielded(1));
    // cmd 2: nested yield, then fall through to the next iteration.
    assert_eq!(c.resume(2), CoroutineState::Yielded(1001));
    assert_eq!(c.resume(0), CoroutineState::Yielded(2));
    // cmd 1: early return from the arm.
    assert_eq!(c.resume(1), CoroutineState::Complete(200));
}

/// Three constructs deep: if inside for inside while, with the yield at
/// the innermost level and loop-carried variables at each level.
#[baregen::coroutine(yield = u32)]
fn deep(rows: u32) -> u32 {
    let mut count: u32 = 0;
    let mut row: u32 = 0;
    while row < rows {
        for col in 0u32..3 {
            if (row + col) % 2 == 0 {
                yield_!(row * 10 + col);
                count += 1;
            }
        }
        row += 1;
    }
    count
}

#[test]
fn if_inside_for_inside_while() {
    let mut c = deep(2);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(2));
    assert_eq!(c.resume(()), CoroutineState::Yielded(11));
    assert_eq!(c.resume(()), CoroutineState::Complete(3));
}

/// Labeled continue from an inner loop to an outer for.
#[baregen::coroutine(yield = u32, resume = bool)]
fn scan() -> u32 {
    let mut hits: u32 = 0;
    'rows: for i in 0u32..3 {
        loop {
            let skip = yield_!(i);
            if skip {
                yield_!(100 + i);
                continue 'rows;
            }
            hits += 1;
            break;
        }
    }
    hits
}

#[test]
fn labeled_continue_from_inner_loop_to_outer_for() {
    let mut c = scan();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    // Skip row 0 entirely; the outer for advances.
    assert_eq!(c.resume(true), CoroutineState::Yielded(100));
    assert_eq!(c.resume(false), CoroutineState::Yielded(1));
    assert_eq!(c.resume(false), CoroutineState::Yielded(2));
    assert_eq!(c.resume(false), CoroutineState::Complete(2));
}

// === Poisoning inside control flow ===

fn assert_resume_panics_poisoned<C: Coroutine<R>, R>(c: &mut C, resume: R) {
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        c.resume(resume);
    }));
    let msg = *poisoned.unwrap_err().downcast::<&str>().unwrap();
    assert_eq!(msg, "Poisoned");
}

/// User code panicking in a for body after a yield.
#[baregen::coroutine(yield = u32, resume = bool)]
fn fragile_for() -> u32 {
    let mut done: u32 = 0;
    for i in 0u32..5 {
        let explode = yield_!(i);
        if explode {
            panic!("boom in loop body");
        }
        done += 1;
    }
    done
}

#[test]
fn panic_in_for_body_poisons_the_state() {
    let mut c = fragile_for();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(false), CoroutineState::Yielded(1));
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.resume(true)));
    assert!(caught.is_err());
    assert_resume_panics_poisoned(&mut c, false);
}

/// User code panicking inside a match arm of an expanded match.
#[baregen::coroutine(yield = u32, resume = u32)]
fn fragile_match() -> u32 {
    let cmd = yield_!(0);
    match cmd {
        0 => {
            yield_!(1);
        }
        _ => panic!("boom in match arm"),
    }
    7
}

#[test]
fn panic_in_match_arm_poisons_the_state() {
    let mut c = fragile_match();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.resume(9)));
    assert!(caught.is_err());
    assert_resume_panics_poisoned(&mut c, 0);
}

/// User code panicking in a loop condition (an expanded while header).
#[baregen::coroutine(yield = u32, resume = u32)]
fn fragile_while() -> u32 {
    let mut div: u32 = 2;
    let mut total: u32 = 0;
    while total / div < 10 {
        let d = yield_!(total);
        total += 1;
        div -= d;
    }
    total
}

#[test]
fn panic_in_while_condition_poisons_the_state() {
    let mut c = fragile_while();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    // Resuming with 2 zeroes the divisor; the next header evaluation
    // divides by zero.
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.resume(2)));
    assert!(caught.is_err());
    assert_resume_panics_poisoned(&mut c, 1);
}

// === Regressions: derive / generics / reference args × control flow ===

/// A generic value held across a yield inside a for loop.
#[baregen::coroutine(yield = u32)]
fn repeat<T: Clone>(x: T, n: u32) -> Vec<T> {
    let mut out: Vec<T> = Vec::new();
    for i in 0u32..n {
        yield_!(i);
        out.push(x.clone());
    }
    out
}

#[test]
fn generic_value_crosses_a_loop_yield() {
    let mut c = repeat("v".to_string(), 2);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(
        c.resume(()),
        CoroutineState::Complete(vec!["v".to_string(), "v".to_string()])
    );
}

/// derive(Clone, Debug) on a state that stores a mid-iteration Range.
#[baregen::coroutine(yield = u32)]
#[derive(Clone, Debug)]
fn countdown(n: u32) -> u32 {
    let mut sum: u32 = 0;
    for i in 0u32..n {
        yield_!(i);
        sum += i;
    }
    sum
}

#[test]
fn cloned_mid_loop_snapshot_resumes_independently() {
    let mut c = countdown(3);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));

    let mut snapshot = c.clone();
    assert!(format!("{snapshot:?}").contains("S1"));

    for c in [&mut c, &mut snapshot] {
        assert_eq!(c.resume(()), CoroutineState::Yielded(2));
        assert_eq!(c.resume(()), CoroutineState::Complete(3));
    }
}

/// A `&mut` argument used across a yield inside a for loop: the external
/// borrow is stored in the state and used on every iteration.
#[baregen::coroutine(yield = usize)]
fn append_n(out: &mut Vec<u32>, n: u32) {
    for i in 0u32..n {
        yield_!(out.len());
        out.push(i);
    }
}

#[test]
fn mut_reference_arg_crosses_a_loop_yield() {
    let mut v = vec![9];
    {
        let mut c = append_n(&mut v, 2);
        assert_eq!(c.start(), CoroutineState::Yielded(1));
        assert_eq!(c.resume(()), CoroutineState::Yielded(2));
        assert_eq!(c.resume(()), CoroutineState::Complete(()));
    }
    assert_eq!(v, [9, 0, 1]);
}
