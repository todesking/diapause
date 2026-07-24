//! Behavior of the in-place resume arms (`analyze_cfg::InPlacePlan`):
//! observable equivalence with the move-out codegen, the `in_place =
//! false` opt-out, and the panic-semantics trade-off — an in-place arm
//! leaves the suspended state behind on panic (resuming it is safe but
//! unspecified; these tests pin the current behavior), while
//! `in_place = false` restores the panics-leave-`Poisoned` guarantee.

use diapause::{Coroutine, CoroutineState, CoroutineStatus};

// === Observable equivalence ===

/// A large buffer lives across every yield: the shape the in-place arm
/// optimizes. `in_place = false` compiles the same body with the
/// move-out codegen; both must behave identically.
macro_rules! buffer_coroutine {
    ($name:ident $(, $k:ident = $v:literal)?) => {
        #[diapause::coroutine(yield = u64 $(, $k = $v)?)]
        fn $name(n: u64) -> u64 {
            let mut buf: [u64; 32] = [0; 32];
            for i in 0..n {
                let idx: usize = (i & 31) as usize;
                buf[idx] = buf[idx].wrapping_add(i).wrapping_add(1);
                yield_!(buf[idx]);
            }
            let mut sum: u64 = 0;
            for v in buf {
                sum = sum.wrapping_add(v);
            }
            sum
        }
    };
}

buffer_coroutine!(buffered);
buffer_coroutine!(buffered_moving, in_place = false);

fn drive<C: Coroutine<(), Yield = u64, Return = u64>>(mut c: C) -> (Vec<u64>, u64) {
    let mut yields = Vec::new();
    let mut st = c.start();
    loop {
        match st {
            CoroutineState::Yielded(v) => {
                yields.push(v);
                assert_eq!(c.status(), CoroutineStatus::Suspended);
                st = c.resume(());
            }
            CoroutineState::Complete(r) => {
                assert_eq!(c.status(), CoroutineStatus::Done);
                return (yields, r);
            }
        }
    }
}

#[test]
fn in_place_and_move_out_codegen_agree() {
    for n in [0, 1, 31, 32, 100] {
        assert_eq!(drive(buffered(n)), drive(buffered_moving(n)), "n = {n}");
    }
}

/// The resume-value shape: the loop variable is live across the yield
/// (re-bound by the `for` head and written back at the suspension).
macro_rules! total_coroutine {
    ($name:ident $(, $k:ident = $v:literal)?) => {
        #[diapause::coroutine(yield = u64, resume = u64 $(, $k = $v)?)]
        fn $name(n: u64) -> u64 {
            let mut sum: u64 = 0;
            for i in 0..n {
                let bonus = yield_!(sum);
                sum = sum.wrapping_add(i).wrapping_add(bonus);
            }
            sum
        }
    };
}

total_coroutine!(running_total);
total_coroutine!(running_total_moving, in_place = false);

fn drive_total<C: Coroutine<u64, Yield = u64, Return = u64>>(mut c: C) -> (Vec<u64>, u64) {
    let mut yields = Vec::new();
    let mut st = c.start();
    loop {
        match st {
            CoroutineState::Yielded(v) => {
                yields.push(v);
                st = c.resume(v & 0x7);
            }
            CoroutineState::Complete(r) => return (yields, r),
        }
    }
}

#[test]
fn rebound_loop_variable_agrees_with_move_out_codegen() {
    for n in [0, 1, 2, 7, 100] {
        assert_eq!(
            drive_total(running_total(n)),
            drive_total(running_total_moving(n)),
            "n = {n}"
        );
    }
}

// === Suspended-state persistence across the optimization ===

/// The state enum's shape and contents at a suspension point are
/// identical with and without the in-place arms: a state serialized
/// from one codegen resumes correctly under the other.
#[test]
fn serialized_states_are_interchangeable() {
    macro_rules! ser_coroutine {
        ($name:ident $(, $k:ident = $v:literal)?) => {
            #[diapause::coroutine(yield = u64 $(, $k = $v)?)]
            #[derive(serde::Serialize, serde::Deserialize)]
            fn $name(n: u64) -> u64 {
                let mut acc: u64 = 0;
                for i in 0..n {
                    acc = acc.wrapping_add(i);
                    yield_!(acc);
                }
                acc
            }
        };
    }
    ser_coroutine!(ser);
    ser_coroutine!(ser_moving, in_place = false);

    let mut a = ser(5);
    let mut b = ser_moving(5);
    assert_eq!(a.start(), CoroutineState::Yielded(0));
    assert_eq!(b.start(), CoroutineState::Yielded(0));
    assert_eq!(a.resume(()), CoroutineState::Yielded(1));
    assert_eq!(b.resume(()), CoroutineState::Yielded(1));
    let ja = serde_json::to_string(&a).unwrap();
    let jb = serde_json::to_string(&b).unwrap();
    assert_eq!(ja, jb, "suspended representations must match");
    // Cross-resume: each codegen picks up the other's snapshot.
    let mut a2: ser_moving::State = serde_json::from_str(&ja).unwrap();
    let mut b2: ser::State = serde_json::from_str(&jb).unwrap();
    assert_eq!(a2.resume(()), CoroutineState::Yielded(3));
    assert_eq!(b2.resume(()), CoroutineState::Yielded(3));
}

// === Panic semantics ===

macro_rules! fragile_coroutine {
    ($name:ident $(, $k:ident = $v:literal)?) => {
        #[diapause::coroutine(yield = u64, resume = bool $(, $k = $v)?)]
        fn $name() -> u64 {
            let mut count: u64 = 0;
            loop {
                let explode = yield_!(count);
                if explode {
                    panic!("boom");
                }
                count += 1;
            }
        }
    };
}

fragile_coroutine!(fragile);
fragile_coroutine!(fragile_moving, in_place = false);

#[test]
fn panic_in_an_in_place_arm_leaves_the_state_suspended() {
    let mut c = fragile();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(false), CoroutineState::Yielded(1));
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.resume(true)));
    assert!(caught.is_err());
    // Documented trade-off: not Poisoned. Resuming is memory-safe;
    // the concrete behavior (a re-run here, since the panic preceded
    // any mutation) is unspecified but pinned by this test.
    assert_eq!(c.status(), CoroutineStatus::Suspended);
    assert_eq!(c.resume(false), CoroutineState::Yielded(2));
}

#[test]
fn panic_with_in_place_disabled_poisons_the_state() {
    let mut c = fragile_moving();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(false), CoroutineState::Yielded(1));
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.resume(true)));
    assert!(caught.is_err());
    assert_eq!(c.status(), CoroutineStatus::Poisoned);
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.resume(false)));
    let msg = *poisoned.unwrap_err().downcast::<&str>().unwrap();
    assert_eq!(msg, "Poisoned");
}

/// A panic on a completion path (past the hot region) still poisons:
/// the state was already moved back out when the panic fired.
#[test]
fn panic_on_a_completion_path_still_poisons() {
    #[diapause::coroutine(yield = u64, resume = u64)]
    fn finisher() -> u64 {
        let mut sum: u64 = 0;
        for i in 0..3u64 {
            let d = yield_!(sum);
            sum += i + d;
        }
        // Cold path: runs after rehydration (and may freely mention
        // `sum` inside a macro — the rewrite never touches cold code).
        assert_ne!(sum, u64::MAX, "unreachable");
        let zero: u64 = std::hint::black_box(0);
        100 / zero
    }
    let mut c = finisher();
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(0), CoroutineState::Yielded(0));
    assert_eq!(c.resume(0), CoroutineState::Yielded(1));
    // The last resume exhausts the loop and panics computing the
    // completion value (division by zero).
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.resume(0)));
    assert!(caught.is_err());
    assert_eq!(c.status(), CoroutineStatus::Poisoned);
}
