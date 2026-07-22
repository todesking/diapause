//! Trait-level API surface: `try_start`/`try_resume`, the `&mut C`
//! forwarding impl, and `dyn Coroutine` compatibility.

use diapause::{Coroutine, CoroutineState, CoroutineStatus};

#[diapause::coroutine(yield = u32, resume = u32)]
fn adder(start: u32) -> u32 {
    let a = yield_!(start);
    start + a
}

/// `try_start`/`try_resume` succeed exactly when `status()` says the
/// corresponding panicking call would, and report the offending status
/// otherwise.
#[test]
fn try_start_and_try_resume_follow_status() {
    let mut c = adder(10);
    assert_eq!(c.try_resume(1), Err(CoroutineStatus::NotStarted));
    assert_eq!(c.try_start(), Ok(CoroutineState::Yielded(10)));
    assert_eq!(c.try_start(), Err(CoroutineStatus::Suspended));
    assert_eq!(c.try_resume(5), Ok(CoroutineState::Complete(15)));
    assert_eq!(c.try_resume(5), Err(CoroutineStatus::Done));
    assert_eq!(c.try_start(), Err(CoroutineStatus::Done));
}

/// A generic driver can take the coroutine by `&mut` thanks to the
/// forwarding impl, leaving the caller in possession of the state.
#[test]
fn coroutine_impl_for_mut_ref() {
    fn drive<C: Coroutine<u32, Yield = u32, Return = u32>>(mut c: C) -> u32 {
        let mut r = c.start();
        loop {
            match r {
                CoroutineState::Yielded(y) => r = c.resume(y + 1),
                CoroutineState::Complete(v) => return v,
            }
        }
    }

    let mut c = adder(10);
    assert_eq!(drive(&mut c), 21);
    assert!(c.is_done());
}

/// `Coroutine` is dyn-compatible: state machines with equal yield,
/// resume, and return types can be driven through `dyn Coroutine`.
#[test]
fn coroutine_is_dyn_compatible() {
    #[diapause::coroutine(yield = u32, resume = u32)]
    fn doubler(start: u32) -> u32 {
        let a = yield_!(start * 2);
        a * 2
    }

    let mut a = adder(1);
    let mut b = doubler(1);
    let coroutines: [&mut dyn Coroutine<u32, Yield = u32, Return = u32>; 2] = [&mut a, &mut b];
    let mut results = Vec::new();
    for c in coroutines {
        assert_eq!(c.status(), CoroutineStatus::NotStarted);
        let CoroutineState::Yielded(y) = c.start() else {
            panic!("expected a yield");
        };
        let CoroutineState::Complete(v) = c.resume(y) else {
            panic!("expected completion");
        };
        results.push(v);
    }
    assert_eq!(results, [2, 4]);

    let mut boxed: Box<dyn Coroutine<u32, Yield = u32, Return = u32>> = Box::new(adder(3));
    assert_eq!(boxed.start(), CoroutineState::Yielded(3));
    assert_eq!(boxed.resume(4), CoroutineState::Complete(7));
}
