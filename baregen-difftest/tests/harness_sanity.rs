//! Checks the differential harness itself: a hand-written equivalent
//! coroutine/reference pair must pass, and a deliberately diverging
//! reference must be caught — otherwise the property tests could be
//! passing vacuously.

use baregen_difftest::{check_case, yield_};

#[baregen::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn running_total(n: u32) -> u32 {
    let mut sum: u32 = 0u32;
    for i in 0u32..n {
        let bonus = yield_!(sum);
        sum = sum.wrapping_add(i).wrapping_add(bonus);
    }
    sum
}

fn running_total_ref(n: u32) -> u32 {
    let mut sum: u32 = 0u32;
    for i in 0u32..n {
        let bonus = yield_!(sum);
        sum = sum.wrapping_add(i).wrapping_add(bonus);
    }
    sum
}

#[test]
fn harness_accepts_equivalent_pair() {
    for resumes in [vec![0], vec![1, 2, 3], vec![7, 7]] {
        check_case(
            "running_total",
            &[4],
            &resumes,
            || running_total_ref(4),
            running_total(4),
        );
    }
}

#[test]
fn harness_accepts_yield_free_run() {
    // n = 0 completes on start() without suspending once.
    check_case(
        "running_total(0)",
        &[0],
        &[1],
        || running_total_ref(0),
        running_total(0),
    );
}

#[test]
fn harness_detects_divergence() {
    fn wrong_ref(n: u32) -> u32 {
        let mut sum: u32 = 0u32;
        for i in 0u32..n {
            let bonus = yield_!(sum.wrapping_add(1u32));
            sum = sum.wrapping_add(i).wrapping_add(bonus);
        }
        sum
    }
    let result = std::panic::catch_unwind(|| {
        check_case("wrong", &[4], &[1, 2], || wrong_ref(4), running_total(4))
    });
    assert!(
        result.is_err(),
        "harness failed to detect a diverging reference"
    );
}
