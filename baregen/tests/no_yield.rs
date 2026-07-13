//! Coroutines without any `yield_!`: `start()` runs the whole body and
//! returns `Complete`.

use baregen::{Coroutine, CoroutineState};

#[baregen::coroutine]
fn no_args() {}

#[baregen::coroutine]
fn add(a: u32, b: u32) -> u32 {
    a + b
}

#[baregen::coroutine]
fn mut_arg(mut a: u32) -> u32 {
    a += 1;
    a
}

struct Value {
    n: i32,
}

#[baregen::coroutine]
fn takes_struct(v: Value) -> i32 {
    v.n
}

mod outer {
    #[baregen::coroutine]
    pub fn double(x: i32) -> i32 {
        x * 2
    }
}

#[test]
fn start_completes_unit() {
    let mut c = no_args();
    assert_eq!(c.start(), CoroutineState::Complete(()));
}

#[test]
fn start_runs_body_with_args() {
    let mut c = add(2, 3);
    assert_eq!(c.start(), CoroutineState::Complete(5));
}

#[test]
fn mut_binding_is_preserved() {
    let mut c = mut_arg(41);
    assert_eq!(c.start(), CoroutineState::Complete(42));
}

#[test]
fn arg_types_resolve_in_generated_mod() {
    let mut c = takes_struct(Value { n: 7 });
    assert_eq!(c.start(), CoroutineState::Complete(7));
}

#[test]
fn visibility_is_propagated() {
    let mut c = outer::double(21);
    assert_eq!(c.start(), CoroutineState::Complete(42));
}

#[test]
#[should_panic(expected = "Not started")]
fn resume_before_start_panics() {
    let mut c = add(1, 2);
    c.resume(());
}

#[test]
#[should_panic(expected = "Already started")]
fn double_start_panics() {
    let mut c = add(1, 2);
    c.start();
    c.start();
}

#[test]
#[should_panic(expected = "Already done")]
fn resume_after_done_panics() {
    let mut c = add(1, 2);
    c.start();
    c.resume(());
}
