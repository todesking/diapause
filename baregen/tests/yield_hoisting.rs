//! Expression-position yields hoisted out of an evaluation-order
//! prefix: every `yield_!` preceded only by paths, literals, and other
//! yields is rewritten into `let __tmpN = yield_!(..);` in front of the
//! statement, with the expression reading `__tmpN` instead.

use baregen::{Coroutine, CoroutineState};

// === Call arguments ===

fn combine(x: u32, y: u32, z: u32, w: u32) -> u32 {
    x * 1000 + y * 100 + z * 10 + w
}

fn seven() -> u32 {
    7
}

/// Two yields form the prefix; the later impure arguments run after
/// them. `__tmp0` crosses the second yield and is stored in the state
/// with the attribute's resume type.
#[baregen::coroutine(yield = u32, resume = u32)]
fn call_args(a: u32) -> u32 {
    combine(yield_!(a), yield_!(a + 1), seven(), a)
}

#[test]
fn yields_in_call_arguments() {
    let mut c = call_args(3);
    assert_eq!(c.start(), CoroutineState::Yielded(3));
    assert_eq!(c.resume(1), CoroutineState::Yielded(4));
    assert_eq!(c.resume(2), CoroutineState::Complete(1273));
}

/// The hoisted temporary crossing a later yield works for non-trivial
/// resume types too (the binding defaults to the attribute's resume
/// type).
fn join(a: String, b: String) -> String {
    a + &b
}

#[baregen::coroutine(yield = u32, resume = String)]
fn string_resume() -> String {
    join(yield_!(1), yield_!(2))
}

#[test]
fn hoisted_tmp_crosses_a_yield_with_a_non_copy_resume_type() {
    let mut c = string_resume();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume("ab".to_string()), CoroutineState::Yielded(2));
    assert_eq!(
        c.resume("cd".to_string()),
        CoroutineState::Complete("abcd".to_string())
    );
}

// === Operators ===

#[baregen::coroutine(yield = u32, resume = u32)]
fn binary_operand(x: u32) -> u32 {
    let v = yield_!(x) + 2;
    v * 10
}

#[test]
fn yield_as_a_binary_operand() {
    let mut c = binary_operand(1);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(5), CoroutineState::Complete(70));
}

// === Assignments ===

fn double(v: u32) -> u32 {
    v * 2
}

#[baregen::coroutine(yield = u32, resume = u32)]
fn assignments() -> u32 {
    let mut x: u32 = 3;
    x = double(yield_!(1)) + x;
    x += yield_!(2);
    x
}

#[test]
fn yields_in_assignment_rhs_and_compound_assignment() {
    let mut c = assignments();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(10), CoroutineState::Yielded(2));
    assert_eq!(c.resume(4), CoroutineState::Complete(27));
}

// === Trailing expressions ===

#[baregen::coroutine(yield = u32, resume = u32)]
fn tail_yield(n: u32) -> u32 {
    yield_!(n)
}

#[test]
fn trailing_yield_returns_the_resume_value() {
    let mut c = tail_yield(4);
    assert_eq!(c.start(), CoroutineState::Yielded(4));
    assert_eq!(c.resume(7), CoroutineState::Complete(7));
}

#[baregen::coroutine(yield = u32, resume = u32)]
fn nested_yield() -> u32 {
    yield_!(yield_!(1))
}

#[test]
fn nested_yields_hoist_inside_out() {
    let mut c = nested_yield();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(5), CoroutineState::Yielded(5));
    assert_eq!(c.resume(9), CoroutineState::Complete(9));
}

#[baregen::coroutine(yield = u32, resume = u32)]
fn tail_receiver() -> u32 {
    yield_!(1).min(50)
}

#[test]
fn yield_as_a_method_receiver() {
    let mut c = tail_receiver();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(60), CoroutineState::Complete(50));
}

// === Conditions and scrutinees ===

#[baregen::coroutine(yield = u32, resume = u32)]
fn if_condition() -> u32 {
    let mut acc: u32 = 0;
    if yield_!(1) > 10 {
        acc += yield_!(2);
    }
    acc
}

#[test]
fn yield_in_an_if_condition() {
    let mut c = if_condition();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(11), CoroutineState::Yielded(2));
    assert_eq!(c.resume(5), CoroutineState::Complete(5));

    let mut c = if_condition();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(10), CoroutineState::Complete(0));
}

#[baregen::coroutine(yield = u32, resume = u32)]
fn if_let_scrutinee() -> u32 {
    let mut out: u32 = 100;
    if let 0 = yield_!(1) % 2 {
        out += yield_!(2);
    }
    out
}

#[test]
fn yield_in_an_if_let_scrutinee() {
    let mut c = if_let_scrutinee();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(4), CoroutineState::Yielded(2));
    assert_eq!(c.resume(8), CoroutineState::Complete(108));

    let mut c = if_let_scrutinee();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(3), CoroutineState::Complete(100));
}

/// A trailing `match` on a hoisted scrutinee, with a bare (non-block)
/// yield arm body that gets wrapped into a block.
#[baregen::coroutine(yield = u32, resume = u32)]
fn match_scrutinee() -> u32 {
    match yield_!(1) % 2 {
        0 => 10,
        _ => yield_!(2),
    }
}

#[test]
fn yield_in_a_match_scrutinee_and_a_bare_arm() {
    let mut c = match_scrutinee();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(4), CoroutineState::Complete(10));

    let mut c = match_scrutinee();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(3), CoroutineState::Yielded(2));
    assert_eq!(c.resume(42), CoroutineState::Complete(42));
}

#[baregen::coroutine(yield = u32, resume = u32)]
fn let_else_scrutinee() -> u32 {
    let 0..=9 = yield_!(1) else {
        yield_!(99);
        return 0;
    };
    1
}

#[test]
fn yield_in_a_let_else_scrutinee() {
    let mut c = let_else_scrutinee();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(5), CoroutineState::Complete(1));

    let mut c = let_else_scrutinee();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(50), CoroutineState::Yielded(99));
    assert_eq!(c.resume(0), CoroutineState::Complete(0));
}

// === for heads ===

#[baregen::coroutine(yield = u32, resume = u32)]
fn for_head() -> u32 {
    let mut sum: u32 = 0;
    for i in 0u32..yield_!(1) {
        sum += yield_!(i);
    }
    sum
}

#[test]
fn yield_in_a_for_head() {
    let mut c = for_head();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(2), CoroutineState::Yielded(0));
    assert_eq!(c.resume(10), CoroutineState::Yielded(1));
    assert_eq!(c.resume(20), CoroutineState::Complete(30));
}

// === Composite values ===

#[baregen::coroutine(yield = u32, resume = u32)]
fn tuple_of_yields() -> u32 {
    let pair = (yield_!(1), yield_!(2));
    pair.0 * 10 + pair.1
}

#[test]
fn tuple_construction_between_yields_is_pure() {
    let mut c = tuple_of_yields();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(3), CoroutineState::Yielded(2));
    assert_eq!(c.resume(4), CoroutineState::Complete(34));
}

/// A bare yield arm body in a statement-position (discarding) match:
/// the wrapped `{ let __tmpN = yield_!(..); __tmpN }` leaves a path
/// statement behind, which the generated code allows.
#[baregen::coroutine(yield = u32, resume = u32)]
fn discarded_bare_arm(k: u32) -> u32 {
    match k {
        0 => yield_!(10),
        _ => yield_!(20),
    }
    k + 1
}

#[test]
fn bare_yield_arm_in_a_discarding_match() {
    let mut c = discarded_bare_arm(0);
    assert_eq!(c.start(), CoroutineState::Yielded(10));
    assert_eq!(c.resume(5), CoroutineState::Complete(1));

    let mut c = discarded_bare_arm(2);
    assert_eq!(c.start(), CoroutineState::Yielded(20));
    assert_eq!(c.resume(5), CoroutineState::Complete(3));
}
