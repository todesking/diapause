//! `yield_all!`'s call-expression operand: `yield_all!(sub(x))` runs a
//! freshly created coroutine without binding it first. The delegate's
//! type is derived from the callee path (`sub` -> `sub::State`), so the
//! two-line form and this one produce the same state machine.

use diapause::{Coroutine, CoroutineState};

#[diapause::coroutine(yield = u32, resume = u32)]
fn inner_sum(start: u32) -> u32 {
    let a = yield_!(start);
    let b = yield_!(start + a);
    start + a + b
}

#[diapause::coroutine(yield = u32)]
#[derive(Clone)]
fn count_to(n: u32) {
    for i in 0u32..n {
        yield_!(i);
    }
}

#[diapause::coroutine(yield = u32, resume = u32)]
fn let_initializer(n: u32) -> u32 {
    let sub: u32 = yield_all!(inner_sum(n));
    sub + 1
}

#[test]
fn call_operand_as_a_let_initializer() {
    let mut c = let_initializer(1);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(2), CoroutineState::Yielded(3));
    // The delegate completes with 1+2+3, bound to `sub`.
    assert_eq!(c.resume(3), CoroutineState::Complete(7));
}

/// The completion binding needs no annotation here either: its type is
/// derived from the delegate's, itself derived from the callee path.
#[diapause::coroutine(yield = u32, resume = u32)]
fn unannotated_completion(n: u32) -> u32 {
    let sub = yield_all!(inner_sum(n));
    sub + 1
}

#[test]
fn call_operand_with_an_unannotated_completion_binding() {
    let mut c = unannotated_completion(1);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(2), CoroutineState::Yielded(3));
    assert_eq!(c.resume(3), CoroutineState::Complete(7));
}

#[diapause::coroutine(yield = u32)]
fn statement_position(n: u32) -> u32 {
    yield_all!(count_to(n));
    99
}

#[test]
fn call_operand_in_statement_position_discards_the_completion_value() {
    let mut c = statement_position(2);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(99));
}

#[diapause::coroutine(yield = u32, resume = u32)]
fn tail_position(n: u32) -> u32 {
    yield_all!(inner_sum(n))
}

#[test]
fn call_operand_as_the_trailing_expression() {
    let mut c = tail_position(100);
    assert_eq!(c.start(), CoroutineState::Yielded(100));
    assert_eq!(c.resume(1), CoroutineState::Yielded(101));
    assert_eq!(c.resume(2), CoroutineState::Complete(103));
}

/// A match arm in tail position, the recursive case of the position
/// rule — the call operand rides on the same desugaring.
#[diapause::coroutine(yield = u32, resume = u32)]
fn tail_match_arm(n: u32) -> u32 {
    match n {
        0 => 7,
        _ => yield_all!(inner_sum(n)),
    }
}

#[test]
fn call_operand_at_a_match_arm_tail() {
    let mut c = tail_match_arm(0);
    assert_eq!(c.start(), CoroutineState::Complete(7));
    let mut c = tail_match_arm(1);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(2), CoroutineState::Yielded(3));
    assert_eq!(c.resume(3), CoroutineState::Complete(6));
}

#[diapause::coroutine(yield = u32, resume = u32)]
fn fallible_inner(start: u32) -> Result<u32, String> {
    let a = yield_!(start);
    if a == 0 {
        return Err("zero".to_string());
    }
    Ok(start + a)
}

#[diapause::coroutine(yield = u32, resume = u32)]
fn call_operand_with_try(n: u32) -> Result<u32, String> {
    let v: u32 = yield_all!(fallible_inner(n))?;
    Ok(v + 1)
}

#[test]
fn try_operator_applies_to_a_call_operand() {
    let mut c = call_operand_with_try(1);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(2), CoroutineState::Complete(Ok(4)));

    let mut c = call_operand_with_try(1);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(
        c.resume(0),
        CoroutineState::Complete(Err("zero".to_string()))
    );
}

/// The `box` modifier composes with the call operand, which is what
/// makes a one-line recursive delegation possible.
#[diapause::coroutine(yield = u32, resume = u32)]
fn countdown(n: u32) -> u32 {
    yield_!(n);
    if n == 0 {
        0
    } else {
        let v: u32 = yield_all!(box countdown(n - 1));
        v + 1
    }
}

#[test]
fn boxed_recursive_delegation_from_a_call_operand() {
    let mut c = countdown(2);
    assert_eq!(c.start(), CoroutineState::Yielded(2));
    assert_eq!(c.resume(0), CoroutineState::Yielded(1));
    assert_eq!(c.resume(0), CoroutineState::Yielded(0));
    assert_eq!(c.resume(0), CoroutineState::Complete(2));
}

#[diapause::coroutine(yield = u32)]
fn generic_inner<T: Clone>(_x: T) -> u32 {
    yield_!(100);
    200
}

/// A generic coroutine's type parameters are not deducible from the
/// call's surface syntax, so they are spelled with a turbofish, which
/// becomes the state type's arguments (`generic_inner::State<T>`).
#[diapause::coroutine(yield = u32)]
fn turbofish_operand<T: Clone>(x: T) -> u32 {
    yield_all!(generic_inner::<T>(x))
}

#[test]
fn turbofish_carries_over_to_the_delegate_type() {
    let mut c = turbofish_operand("test".to_string());
    assert_eq!(c.start(), CoroutineState::Yielded(100));
    assert_eq!(c.resume(()), CoroutineState::Complete(200));

    let mut c = turbofish_operand(42i32);
    assert_eq!(c.start(), CoroutineState::Yielded(100));
    assert_eq!(c.resume(()), CoroutineState::Complete(200));
}

mod nested {
    #[diapause::coroutine(yield = u32, resume = u32)]
    pub fn doubler(start: u32) -> u32 {
        let a = yield_!(start);
        start + a
    }
}

/// A path-qualified callee keeps its prefix (`nested::doubler::State`),
/// and a `use ... as` alias resolves because the import brings both the
/// starter function and the module of the same name into scope.
use nested::doubler as aliased;

#[diapause::coroutine(yield = u32, resume = u32)]
fn qualified_and_aliased_callees(n: u32) -> u32 {
    let a: u32 = yield_all!(nested::doubler(n));
    yield_all!(aliased(a))
}

#[test]
fn qualified_and_aliased_call_operands() {
    let mut c = qualified_and_aliased_callees(1);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    // First delegation completes with 1+2, entering the second with it.
    assert_eq!(c.resume(2), CoroutineState::Yielded(3));
    assert_eq!(c.resume(4), CoroutineState::Complete(7));
}

/// The delegate is stored in the state under its derived type like any
/// other, so derives still compose across the nesting.
#[diapause::coroutine(yield = u32)]
#[derive(Clone)]
fn cloneable_outer(n: u32) -> u32 {
    yield_all!(count_to(n));
    n
}

#[test]
fn a_delegation_from_a_call_operand_can_be_cloned() {
    let mut c = cloneable_outer(2);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    let mut snapshot = c.clone();
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(snapshot.resume(()), CoroutineState::Yielded(1));
}

/// Arguments are ordinary expressions evaluated where the delegation
/// starts, including reads of variables held in the state.
#[diapause::coroutine(yield = u32, resume = u32)]
fn arguments_read_locals(n: u32) -> u32 {
    let first = yield_!(n);
    let v: u32 = yield_all!(inner_sum(first + n));
    v + first
}

#[test]
fn call_operand_arguments_read_state_variables() {
    let mut c = arguments_read_locals(1);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    // `first` = 10, so the delegate starts at 11.
    assert_eq!(c.resume(10), CoroutineState::Yielded(11));
    assert_eq!(c.resume(1), CoroutineState::Yielded(12));
    // The delegate completes with 11+1+2 = 14, plus `first`.
    assert_eq!(c.resume(2), CoroutineState::Complete(24));
}

/// Two delegations in a row from call operands get distinct synthetic
/// names, exactly like the variable form.
#[diapause::coroutine(yield = u32)]
fn sequential(n: u32) -> u32 {
    yield_all!(count_to(n));
    yield_all!(count_to(n));
    n
}

#[test]
fn sequential_call_operand_delegations() {
    let mut c = sequential(2);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Yielded(0));
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(2));
}
