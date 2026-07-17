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

// An Option-returning pair exercising `?`, `let else`, and `while let`
// — the same shapes the generator emits for the OptionU32 flavor.
#[baregen::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn checked_sum(n: u32) -> Option<u32> {
    let mut acc: u32 = 0u32;
    let mut o: Option<u32> = Some(n);
    while let Some(p) = o {
        let x: u32 = p;
        o = if x < 3u32 {
            Some(x.wrapping_add(1u32))
        } else {
            None
        };
        let r = yield_!(x);
        acc = acc.wrapping_add(r);
        let 0u32 = r % 4u32 else {
            return None;
        };
    }
    let o2: Option<u32> = Some(acc);
    let doubled: u32 = o2?;
    Some(doubled.wrapping_mul(2u32))
}

fn checked_sum_ref(n: u32) -> Option<u32> {
    let mut acc: u32 = 0u32;
    let mut o: Option<u32> = Some(n);
    while let Some(p) = o {
        let x: u32 = p;
        o = if x < 3u32 {
            Some(x.wrapping_add(1u32))
        } else {
            None
        };
        let r = yield_!(x);
        acc = acc.wrapping_add(r);
        let 0u32 = r % 4u32 else {
            return None;
        };
    }
    let o2: Option<u32> = Some(acc);
    let doubled: u32 = o2?;
    Some(doubled.wrapping_mul(2u32))
}

#[test]
fn harness_accepts_option_flavor_pair() {
    for resumes in [vec![0], vec![4, 8, 0], vec![1], vec![0, 3]] {
        check_case(
            "checked_sum",
            &[2],
            &resumes,
            || checked_sum_ref(2),
            checked_sum(2),
        );
    }
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
