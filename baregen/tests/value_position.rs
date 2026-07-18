//! Value-position yield (v2.1): `let` initializers that are
//! if/match/loop/block expressions containing yields, value-carrying
//! `break`, and control flow as the function's trailing expression.

use baregen::{Coroutine, CoroutineState};

// === let + if ===

#[baregen::coroutine(yield = u32, resume = u32)]
fn pick_if(c: bool) -> u32 {
    let x: u32 = if c {
        let r = yield_!(1);
        r + 10
    } else {
        2
    };
    x * 100
}

#[test]
fn let_if_taken() {
    let mut c = pick_if(true);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(5), CoroutineState::Complete(1500));
}

#[test]
fn let_if_skipped() {
    let mut c = pick_if(false);
    assert_eq!(c.start(), CoroutineState::Complete(200));
}

/// The result binding is mutable and crosses a later yield, so it is
/// stored in a state variant and unpacked `mut`.
#[baregen::coroutine(yield = u32, resume = u32)]
fn mutable_result(c: bool) -> u32 {
    let mut x: u32 = if c {
        let r = yield_!(1);
        r
    } else {
        5
    };
    x += 1;
    yield_!(x);
    x
}

#[test]
fn let_if_mut_binding_crosses_a_later_yield() {
    let mut c = mutable_result(true);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(9), CoroutineState::Yielded(10));
    assert_eq!(c.resume(0), CoroutineState::Complete(10));

    let mut c = mutable_result(false);
    assert_eq!(c.start(), CoroutineState::Yielded(6));
    assert_eq!(c.resume(0), CoroutineState::Complete(6));
}

/// `if` without `else` in value position: the false edge produces `()`.
#[baregen::coroutine(yield = u32)]
fn unit_if(c: bool) -> u32 {
    let x: () = if c {
        yield_!(1);
    };
    let () = x;
    9
}

#[test]
fn let_if_without_else() {
    let mut c = unit_if(true);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(9));
    let mut c = unit_if(false);
    assert_eq!(c.start(), CoroutineState::Complete(9));
}

// === let + match ===

#[baregen::coroutine(yield = u32, resume = u32)]
fn pick_match(k: u32) -> u32 {
    let x: u32 = match k {
        0 => {
            let r = yield_!(10);
            r
        }
        1 => 111,
        n => {
            // The arm binding may not cross the yield; rebind first.
            let n2: u32 = n;
            yield_!(n2);
            n2 + 1
        }
    };
    x + 1000
}

#[test]
fn let_match_yielding_arm_with_resume_value() {
    let mut c = pick_match(0);
    assert_eq!(c.start(), CoroutineState::Yielded(10));
    assert_eq!(c.resume(7), CoroutineState::Complete(1007));
}

#[test]
fn let_match_opaque_arm() {
    let mut c = pick_match(1);
    assert_eq!(c.start(), CoroutineState::Complete(1111));
}

#[test]
fn let_match_arm_consuming_its_pattern_binding() {
    let mut c = pick_match(5);
    assert_eq!(c.start(), CoroutineState::Yielded(5));
    assert_eq!(c.resume(0), CoroutineState::Complete(1006));
}

// === let + loop with value-carrying break ===

#[baregen::coroutine(yield = u32, resume = u32)]
fn accumulate_until(limit: u32) -> u32 {
    let mut acc: u32 = 0;
    let total: u32 = loop {
        let r = yield_!(acc);
        acc += r;
        if acc >= limit {
            yield_!(999);
            break acc;
        }
    };
    total * 2
}

#[test]
fn let_loop_break_value() {
    let mut c = accumulate_until(10);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(4), CoroutineState::Yielded(4));
    assert_eq!(c.resume(7), CoroutineState::Yielded(999));
    assert_eq!(c.resume(0), CoroutineState::Complete(22));
}

// === let + labeled block ===

#[baregen::coroutine(yield = u32)]
fn labeled_block(c: bool) -> u32 {
    let x: u32 = 'b: {
        yield_!(1);
        if c {
            yield_!(2);
            break 'b 10;
        }
        20
    };
    x + 1
}

#[test]
fn let_labeled_block_break_value() {
    let mut c = labeled_block(true);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Yielded(2));
    assert_eq!(c.resume(()), CoroutineState::Complete(11));
}

#[test]
fn let_labeled_block_tail_value() {
    let mut c = labeled_block(false);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(21));
}

// === Nesting ===

/// A match arm whose tail is an `if`, and another arm containing a
/// nested value-`loop` `let` before its own tail.
#[baregen::coroutine(yield = u32, resume = u32)]
fn nested(k: u32) -> u32 {
    let x: u32 = match k {
        0 => {
            if k == 0 {
                let r = yield_!(0);
                r
            } else {
                1
            }
        }
        _ => {
            let y: u32 = loop {
                let r = yield_!(5);
                break r + 5;
            };
            y * 2
        }
    };
    x + 100
}

#[test]
fn nested_if_inside_match_arm() {
    let mut c = nested(0);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(7), CoroutineState::Complete(107));
}

#[test]
fn nested_value_loop_inside_match_arm() {
    let mut c = nested(1);
    assert_eq!(c.start(), CoroutineState::Yielded(5));
    assert_eq!(c.resume(3), CoroutineState::Complete(116));
}

// === Function-tail control flow ===

#[baregen::coroutine(yield = u32, resume = u32)]
fn tail_if(c: bool) -> u32 {
    if c {
        let r = yield_!(1);
        r + 1
    } else {
        yield_!(2);
        20
    }
}

#[test]
fn fn_tail_if_arms_complete_directly() {
    let mut c = tail_if(true);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(5), CoroutineState::Complete(6));

    let mut c = tail_if(false);
    assert_eq!(c.start(), CoroutineState::Yielded(2));
    assert_eq!(c.resume(0), CoroutineState::Complete(20));
}

#[baregen::coroutine(yield = u32, resume = u32)]
fn tail_match(k: u32) -> u32 {
    match k {
        0 => {
            yield_!(0);
            1
        }
        n => n * 2,
    }
}

#[test]
fn fn_tail_match() {
    let mut c = tail_match(0);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(0), CoroutineState::Complete(1));

    let mut c = tail_match(21);
    assert_eq!(c.start(), CoroutineState::Complete(42));
}

#[baregen::coroutine(yield = u32, resume = u32)]
fn tail_loop() -> u32 {
    let mut acc: u32 = 0;
    loop {
        let r = yield_!(acc);
        if r == 0 {
            yield_!(acc);
            break acc;
        }
        acc += r;
    }
}

#[test]
fn fn_tail_loop_break_value_completes() {
    let mut c = tail_loop();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(4), CoroutineState::Yielded(4));
    assert_eq!(c.resume(0), CoroutineState::Yielded(4));
    assert_eq!(c.resume(0), CoroutineState::Complete(4));
}

// === Parenthesized value positions ===
//
// Parens around a yield-containing expression are transparent: the
// parenthesized forms behave exactly like the bare ones.

#[baregen::coroutine(yield = u32, resume = u32)]
fn paren_tail_if(c: bool) -> u32 {
    (if c {
        let r = yield_!(1);
        r + 1
    } else {
        yield_!(2);
        20
    })
}

#[test]
fn paren_fn_tail_if_matches_the_bare_form() {
    let mut c = paren_tail_if(true);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(5), CoroutineState::Complete(6));

    let mut c = paren_tail_if(false);
    assert_eq!(c.start(), CoroutineState::Yielded(2));
    assert_eq!(c.resume(0), CoroutineState::Complete(20));
}

#[baregen::coroutine(yield = u32, resume = u32)]
fn paren_tail_match(k: u32) -> u32 {
    (match k {
        0 => {
            yield_!(0);
            1
        }
        n => n * 2,
    })
}

#[test]
fn paren_fn_tail_match_matches_the_bare_form() {
    let mut c = paren_tail_match(0);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(0), CoroutineState::Complete(1));

    let mut c = paren_tail_match(21);
    assert_eq!(c.start(), CoroutineState::Complete(42));
}

#[baregen::coroutine(yield = u32, resume = u32)]
fn paren_tail_block(n: u32) -> u32 {
    ({
        let r = yield_!(n);
        r + n
    })
}

#[test]
fn paren_fn_tail_block() {
    let mut c = paren_tail_block(3);
    assert_eq!(c.start(), CoroutineState::Yielded(3));
    assert_eq!(c.resume(4), CoroutineState::Complete(7));
}

/// The condition of a parenthesized tail `if` hoists like the bare form.
#[baregen::coroutine(yield = u32, resume = u32)]
fn paren_tail_if_cond(x: u32) -> u32 {
    (if yield_!(x) == 0 {
        yield_!(1);
        10
    } else {
        20
    })
}

#[test]
fn paren_tail_if_condition_hoists() {
    let mut c = paren_tail_if_cond(5);
    assert_eq!(c.start(), CoroutineState::Yielded(5));
    assert_eq!(c.resume(0), CoroutineState::Yielded(1));
    assert_eq!(c.resume(9), CoroutineState::Complete(10));

    let mut c = paren_tail_if_cond(5);
    assert_eq!(c.start(), CoroutineState::Yielded(5));
    assert_eq!(c.resume(3), CoroutineState::Complete(20));
}

/// A parenthesized yield-containing tail inside a value block.
#[baregen::coroutine(yield = u32, resume = u32)]
fn paren_block_tail(c: bool) -> u32 {
    let x: u32 = {
        (if c {
            let r = yield_!(1);
            r
        } else {
            2
        })
    };
    x * 10
}

#[test]
fn paren_tail_of_a_value_block() {
    let mut c = paren_block_tail(true);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(7), CoroutineState::Complete(70));

    let mut c = paren_block_tail(false);
    assert_eq!(c.start(), CoroutineState::Complete(20));
}

/// Statement-position parenthesized control flow containing yield.
#[baregen::coroutine(yield = u32)]
fn paren_stmt_if(c: bool) -> u32 {
    (if c {
        yield_!(1);
    });
    7
}

#[test]
fn paren_statement_if() {
    let mut c = paren_stmt_if(true);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(7));
    let mut c = paren_stmt_if(false);
    assert_eq!(c.start(), CoroutineState::Complete(7));
}

// === Annotation not required when the binding never enters a state ===

/// Every arm diverges via `break`, so the binding is only ever defined
/// and used inside one dispatch arm and needs no annotation.
#[baregen::coroutine(yield = u32, resume = u32)]
fn no_annotation_needed() -> u32 {
    let out: u32 = loop {
        let x = loop {
            let r = yield_!(1);
            break r * 2;
        };
        break x + 1;
    };
    out
}

#[test]
fn unannotated_binding_confined_to_one_arm() {
    let mut c = no_annotation_needed();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(10), CoroutineState::Complete(21));
}
