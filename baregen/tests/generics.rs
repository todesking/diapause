//! Type parameters, where clauses, reference arguments (with lifetime
//! elision), and `impl Trait` arguments.

use std::fmt::Display;

use baregen::{Coroutine, CoroutineState};

#[baregen::coroutine(yield = String)]
fn show<T: Display>(x: T) -> String {
    yield_!(format!("first: {}", x));
    format!("last: {}", x)
}

#[test]
fn type_param_with_bound() {
    let mut c = show(42);
    assert_eq!(c.start(), CoroutineState::Yielded("first: 42".to_string()));
    assert_eq!(
        c.resume(()),
        CoroutineState::Complete("last: 42".to_string())
    );
}

#[baregen::coroutine(yield = u32)]
fn duplicate<T>(a: T) -> (T, T)
where
    T: Clone,
{
    yield_!(1);
    let b = a.clone();
    (a, b)
}

#[test]
fn where_clause_is_copied() {
    let mut c = duplicate("x".to_string());
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(
        c.resume(()),
        CoroutineState::Complete(("x".to_string(), "x".to_string()))
    );
}

#[baregen::coroutine(yield = i32)]
fn add_into(target: &mut Vec<i32>) {
    target.push(1);
    yield_!(1);
    target.push(2);
}

#[test]
fn elided_reference_arg_lives_across_yield() {
    let mut v = Vec::new();
    {
        let mut c = add_into(&mut v);
        assert_eq!(c.start(), CoroutineState::Yielded(1));
        assert_eq!(c.resume(()), CoroutineState::Complete(()));
    }
    assert_eq!(v, [1, 2]);
}

#[baregen::coroutine(yield = u32)]
fn tail<'x>(s: &'x str) -> &'x str {
    yield_!(1);
    &s[1..]
}

#[test]
fn named_lifetime_in_signature_and_return() {
    let mut c = tail("abc");
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete("bc"));
}

#[baregen::coroutine(yield = i32)]
fn apply(f: impl Fn(i32) -> i32) -> i32 {
    yield_!(1);
    f(41)
}

#[test]
fn impl_trait_arg_becomes_type_param() {
    let mut c = apply(|n| n + 1);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(42));
}

#[baregen::coroutine(yield = i32)]
fn make_default<T: Default + Into<i64>>() -> i64 {
    yield_!(1);
    let v: T = T::default();
    v.into()
}

#[test]
fn body_only_type_param_is_anchored_by_phantom() {
    let mut c = make_default::<u8>();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(0));
}

#[baregen::coroutine(yield = u32)]
fn mixed<'k, T: Display>(x: T, y: &'k mut String, z: &u8) -> usize {
    y.push_str(&format!("{}", x));
    yield_!(1);
    y.push('!');
    y.len() + (*z as usize)
}

#[test]
fn type_params_and_multiple_reference_args() {
    let mut s = String::from(">");
    let mut c = mixed(7, &mut s, &2);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(3 + 2));
    assert_eq!(s, ">7!");
}
