//! The code examples from the transformation-summary document, verbatim
//! where possible, so the documentation and the implementation cannot
//! drift apart. Comments name the section each example comes from.

use std::fmt::Display;

use baregen::{Coroutine, CoroutineState};

// "シンプルなケース(yieldなし)"
#[baregen::coroutine]
pub fn simple() {}

#[test]
fn simple_case() {
    let mut c = simple();
    assert_eq!(c.start(), CoroutineState::Complete(()));
}

// "状態enumへのderive"
#[baregen::coroutine]
#[derive(Clone)]
fn derive_example() {}

#[test]
fn derive_on_state_enum() {
    let c = derive_example();
    let mut copy = c.clone();
    assert_eq!(copy.start(), CoroutineState::Complete(()));
}

// "ジェネリクス・参照引数"
pub struct Value {
    pub foo: i32,
}

#[baregen::coroutine]
fn generics_example<T: Display>(x: T, y: &mut Value) {
    y.foo = 1;
    let _ = format!("{}", x);
}

#[test]
fn generics_and_reference_args() {
    let mut v = Value { foo: 0 };
    let mut c = generics_example("x", &mut v);
    assert_eq!(c.start(), CoroutineState::Complete(()));
    assert_eq!(v.foo, 1);
}

// "resume引数の受け渡し"
#[baregen::coroutine(yield = u32, resume = String)]
fn resume_example() -> usize {
    let r = yield_!(1);
    r.len()
}

#[test]
fn resume_argument_passing() {
    let mut c = resume_example();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume("hello".to_string()), CoroutineState::Complete(5));
}

// "yieldをまたぐ借用"
#[baregen::coroutine(yield = i32)]
fn borrow_example() -> i32 {
    let mut x: Value = Value { foo: 1 };
    let y = &mut x;
    yield_!(123);
    y.foo = 99;
    x.foo
}

#[test]
fn borrow_across_yield() {
    let mut c = borrow_example();
    assert_eq!(c.start(), CoroutineState::Yielded(123));
    assert_eq!(c.resume(()), CoroutineState::Complete(99));
}

// Combined features: generics + multiple yields + borrow reconstruction
// + derive(Clone) snapshot.
#[baregen::coroutine(yield = usize, resume = u32)]
#[derive(Clone)]
fn combo<T: Clone + Into<u32>>(seed: T, mut out: Vec<u32>) -> Vec<u32> {
    let mut acc: u32 = seed.into();
    let p = &mut acc;
    let r = yield_!(out.len());
    *p += r;
    let r2 = yield_!(out.len() + 1);
    acc += r2;
    out.push(acc);
    out
}

#[test]
fn combined_features() {
    let mut c = combo(1u8, vec![0]);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(10), CoroutineState::Yielded(2));

    // Fork the suspended coroutine and drive both to completion.
    let mut snapshot = c.clone();
    assert_eq!(c.resume(100), CoroutineState::Complete(vec![0, 111]));
    assert_eq!(snapshot.resume(200), CoroutineState::Complete(vec![0, 211]));
}
