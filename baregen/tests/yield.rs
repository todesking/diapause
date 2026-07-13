//! State machine behaviour for coroutines containing `yield_!`.

use baregen::{Coroutine, CoroutineState};

#[baregen::coroutine(yield = u32)]
fn counter() {
    yield_!(1);
    yield_!(2);
}

#[test]
fn yields_in_order_then_completes() {
    let mut c = counter();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Yielded(2));
    assert_eq!(c.resume(()), CoroutineState::Complete(()));
}

#[baregen::coroutine(yield = u32, resume = String)]
fn echo_len(prefix: usize) -> usize {
    let r = yield_!(1);
    let first: usize = r.len();
    let r2 = yield_!(2);
    prefix + first + r2.len()
}

#[test]
fn resume_values_are_passed_in() {
    let mut c = echo_len(100);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume("ab".to_string()), CoroutineState::Yielded(2));
    assert_eq!(
        c.resume("cdef".to_string()),
        CoroutineState::Complete(100 + 2 + 4)
    );
}

#[baregen::coroutine(yield = i32)]
fn mutate() -> i32 {
    let mut x: i32 = 1;
    x += 1;
    yield_!(x);
    x += 40;
    x
}

#[test]
fn mutable_var_lives_across_yield() {
    let mut c = mutate();
    assert_eq!(c.start(), CoroutineState::Yielded(2));
    assert_eq!(c.resume(()), CoroutineState::Complete(42));
}

#[test]
fn empty_yield_value_defaults_to_unit() {
    let mut c = unit_yield();
    assert_eq!(c.start(), CoroutineState::Yielded(()));
    assert_eq!(c.resume(()), CoroutineState::Complete(()));
}

#[baregen::coroutine]
fn unit_yield() {
    yield_!();
}

#[baregen::coroutine(yield = u32, resume = bool)]
fn bomb() {
    let explode = yield_!(1);
    if explode {
        panic!("boom");
    }
    yield_!(2);
}

#[test]
fn panic_during_resume_poisons_the_state() {
    let mut c = bomb();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.resume(true)));
    assert!(caught.is_err());
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.resume(false)));
    let msg = *poisoned.unwrap_err().downcast::<&str>().unwrap();
    assert_eq!(msg, "Poisoned");
}

#[test]
#[should_panic(expected = "Not started")]
fn resume_before_start_panics() {
    let mut c = counter();
    c.resume(());
}

#[test]
#[should_panic(expected = "Already done")]
fn resume_after_complete_panics() {
    let mut c = counter();
    c.start();
    c.resume(());
    c.resume(());
    c.resume(());
}
