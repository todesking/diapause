//! Hand-written coroutine/reference pairs for borrows that live across
//! a `yield_!`, probing baregen-macro's borrow-substitution / reborrow
//! machinery before the generator relies on it. Every shape here is one
//! the generator emits (or a slightly stronger variant): shared and
//! mutable direct borrows, borrows of arguments, borrow chains, borrows
//! crossing several yields (including inside loops and value-position
//! blocks), and writes through a `&mut` borrow after a resume.
//!
//! References are never stored in the state: the borrowed binding is
//! stored instead and the borrow is re-established at the head of every
//! arm that uses it (see `state_stores_borrowee_not_borrow`).

// Bodies must stay identical between the coroutine and reference
// worlds, so lint-driven rewrites are not applied here.
#![allow(clippy::manual_is_multiple_of)]
#![allow(clippy::needless_range_loop)]
#![allow(unused_assignments)]

use baregen::{Coroutine, CoroutineState};
use baregen_difftest::{check_case, yield_};

// === A: shared borrow across one yield, borrowee and borrow both used
// after the resume, borrowee free again after the borrow's last use ===

#[baregen::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn shared_borrow(a0: u32) -> u32 {
    let mut x: u32 = a0;
    let y: &u32 = &x;
    let r = yield_!(*y);
    let u: u32 = (*y).wrapping_add(r);
    x = u.wrapping_add(1u32);
    x
}

fn shared_borrow_ref(a0: u32) -> u32 {
    let mut x: u32 = a0;
    let y: &u32 = &x;
    let r = yield_!(*y);
    let u: u32 = (*y).wrapping_add(r);
    x = u.wrapping_add(1u32);
    x
}

#[test]
fn sanity_shared_borrow() {
    for a0 in 0u32..4 {
        for resumes in [vec![0], vec![1, 2, 3], vec![7, 7]] {
            check_case(
                "shared_borrow",
                &[a0],
                &resumes,
                || shared_borrow_ref(a0),
                shared_borrow(a0),
            );
        }
    }
}

// === B: unannotated direct borrow (`let y = &x;`) across a yield ===

#[baregen::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn shared_borrow_unannotated(a0: u32) -> u32 {
    let x: u32 = a0.wrapping_add(3u32);
    let y = &x;
    let r = yield_!(x);
    (*y).wrapping_add(r)
}

fn shared_borrow_unannotated_ref(a0: u32) -> u32 {
    let x: u32 = a0.wrapping_add(3u32);
    let y = &x;
    let r = yield_!(x);
    (*y).wrapping_add(r)
}

#[test]
fn sanity_shared_borrow_unannotated() {
    for a0 in 0u32..4 {
        for resumes in [vec![0], vec![5, 1]] {
            check_case(
                "shared_borrow_unannotated",
                &[a0],
                &resumes,
                || shared_borrow_unannotated_ref(a0),
                shared_borrow_unannotated(a0),
            );
        }
    }
}

// === C: mutable borrow across two yields, writes through the borrow
// between and after the resumes, borrowee read after release ===

#[baregen::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn mut_borrow_write(a0: u32) -> u32 {
    let mut x: u32 = a0;
    let m: &mut u32 = &mut x;
    let r0 = yield_!(*m);
    *m = (*m).wrapping_add(r0);
    let r1 = yield_!(*m);
    *m = (*m).wrapping_mul(r1.wrapping_add(1u32));
    let u: u32 = x;
    u.wrapping_add(x)
}

fn mut_borrow_write_ref(a0: u32) -> u32 {
    let mut x: u32 = a0;
    let m: &mut u32 = &mut x;
    let r0 = yield_!(*m);
    *m = (*m).wrapping_add(r0);
    let r1 = yield_!(*m);
    *m = (*m).wrapping_mul(r1.wrapping_add(1u32));
    let u: u32 = x;
    u.wrapping_add(x)
}

#[test]
fn sanity_mut_borrow_write() {
    for a0 in 0u32..4 {
        for resumes in [vec![0], vec![1, 2, 3], vec![9]] {
            check_case(
                "mut_borrow_write",
                &[a0],
                &resumes,
                || mut_borrow_write_ref(a0),
                mut_borrow_write(a0),
            );
        }
    }
}

// === D: borrow chain (`let z = &y;` where `y = &x`) across a yield ===

#[baregen::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn borrow_chain(a0: u32) -> u32 {
    let x: u32 = a0.wrapping_mul(2u32);
    let y: &u32 = &x;
    let z: &&u32 = &y;
    let r = yield_!(**z);
    (**z).wrapping_add(r)
}

fn borrow_chain_ref(a0: u32) -> u32 {
    let x: u32 = a0.wrapping_mul(2u32);
    let y: &u32 = &x;
    let z: &&u32 = &y;
    let r = yield_!(**z);
    (**z).wrapping_add(r)
}

#[test]
fn sanity_borrow_chain() {
    for a0 in 0u32..4 {
        for resumes in [vec![0], vec![4, 5]] {
            check_case(
                "borrow_chain",
                &[a0],
                &resumes,
                || borrow_chain_ref(a0),
                borrow_chain(a0),
            );
        }
    }
}

// === E: shared borrow of a function argument across a yield ===

#[baregen::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn arg_borrow(a0: u32, a1: u32) -> u32 {
    let y: &u32 = &a0;
    let r = yield_!(*y);
    (*y).wrapping_add(r).wrapping_add(a1)
}

fn arg_borrow_ref(a0: u32, a1: u32) -> u32 {
    let y: &u32 = &a0;
    let r = yield_!(*y);
    (*y).wrapping_add(r).wrapping_add(a1)
}

#[test]
fn sanity_arg_borrow() {
    for a0 in 0u32..4 {
        for resumes in [vec![0], vec![1, 2, 3]] {
            check_case(
                "arg_borrow",
                &[a0, 3],
                &resumes,
                || arg_borrow_ref(a0, 3),
                arg_borrow(a0, 3),
            );
        }
    }
}

// === F: borrow established before a loop, used after yields inside the
// loop body (the reborrow is rebuilt at the loop's resume arms every
// iteration) and once more after the loop ===

#[baregen::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn borrow_across_loop(a0: u32) -> u32 {
    let x: u32 = a0.wrapping_add(1u32);
    let y: &u32 = &x;
    let mut acc: u32 = 0u32;
    let mut c0: u32 = 0u32;
    while c0 < 3u32 {
        c0 = c0.wrapping_add(1u32);
        let r = yield_!(acc);
        acc = acc.wrapping_add((*y).wrapping_add(r));
    }
    acc.wrapping_add(*y)
}

fn borrow_across_loop_ref(a0: u32) -> u32 {
    let x: u32 = a0.wrapping_add(1u32);
    let y: &u32 = &x;
    let mut acc: u32 = 0u32;
    let mut c0: u32 = 0u32;
    while c0 < 3u32 {
        c0 = c0.wrapping_add(1u32);
        let r = yield_!(acc);
        acc = acc.wrapping_add((*y).wrapping_add(r));
    }
    acc.wrapping_add(*y)
}

#[test]
fn sanity_borrow_across_loop() {
    for a0 in 0u32..4 {
        for resumes in [vec![0], vec![1, 2, 3], vec![8, 2]] {
            check_case(
                "borrow_across_loop",
                &[a0],
                &resumes,
                || borrow_across_loop_ref(a0),
                borrow_across_loop(a0),
            );
        }
    }
}

// === G: borrow crossing a yield inside a conditional branch; the
// borrowee is reassigned after the borrow's last use ===

#[baregen::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn borrow_in_branch(a0: u32, a1: u32) -> u32 {
    let mut x: u32 = a0;
    if a1 % 2u32 == 0u32 {
        let y: &u32 = &x;
        let r = yield_!(*y);
        let u: u32 = (*y).wrapping_add(r);
        x = u;
    } else {
        let r = yield_!(x);
        x = x.wrapping_sub(r);
    }
    x.wrapping_add(1u32)
}

fn borrow_in_branch_ref(a0: u32, a1: u32) -> u32 {
    let mut x: u32 = a0;
    if a1 % 2u32 == 0u32 {
        let y: &u32 = &x;
        let r = yield_!(*y);
        let u: u32 = (*y).wrapping_add(r);
        x = u;
    } else {
        let r = yield_!(x);
        x = x.wrapping_sub(r);
    }
    x.wrapping_add(1u32)
}

#[test]
fn sanity_borrow_in_branch() {
    for a0 in 0u32..4 {
        for a1 in 0u32..2 {
            for resumes in [vec![0], vec![1, 2, 3]] {
                check_case(
                    "borrow_in_branch",
                    &[a0, a1],
                    &resumes,
                    || borrow_in_branch_ref(a0, a1),
                    borrow_in_branch(a0, a1),
                );
            }
        }
    }
}

// === H: borrow crossing a yield inside a value-position `if` block ===

#[baregen::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn borrow_in_value_if(a0: u32, a1: u32) -> u32 {
    let x: u32 = a0;
    let v: u32 = if a1 % 2u32 == 0u32 {
        let y: &u32 = &x;
        let r = yield_!(*y);
        (*y).wrapping_add(r)
    } else {
        a1
    };
    v.wrapping_add(x)
}

fn borrow_in_value_if_ref(a0: u32, a1: u32) -> u32 {
    let x: u32 = a0;
    let v: u32 = if a1 % 2u32 == 0u32 {
        let y: &u32 = &x;
        let r = yield_!(*y);
        (*y).wrapping_add(r)
    } else {
        a1
    };
    v.wrapping_add(x)
}

#[test]
fn sanity_borrow_in_value_if() {
    for a0 in 0u32..4 {
        for a1 in 0u32..2 {
            for resumes in [vec![0], vec![6, 1]] {
                check_case(
                    "borrow_in_value_if",
                    &[a0, a1],
                    &resumes,
                    || borrow_in_value_if_ref(a0, a1),
                    borrow_in_value_if(a0, a1),
                );
            }
        }
    }
}

// === I: mutable borrow whose only post-resume action is the write;
// the borrowee is then observed by the completion value ===

#[baregen::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn mut_borrow_then_read_source(a0: u32) -> u32 {
    let mut x: u32 = a0;
    let m: &mut u32 = &mut x;
    let r = yield_!(a0);
    *m = r.wrapping_add(40u32);
    x
}

fn mut_borrow_then_read_source_ref(a0: u32) -> u32 {
    let mut x: u32 = a0;
    let m: &mut u32 = &mut x;
    let r = yield_!(a0);
    *m = r.wrapping_add(40u32);
    x
}

#[test]
fn sanity_mut_borrow_then_read_source() {
    for a0 in 0u32..4 {
        for resumes in [vec![0], vec![1, 2, 3]] {
            check_case(
                "mut_borrow_then_read_source",
                &[a0],
                &resumes,
                || mut_borrow_then_read_source_ref(a0),
                mut_borrow_then_read_source(a0),
            );
        }
    }
}

// === Serde shape: the suspended state stores the borrowee (`x`), never
// the borrow (`y`), so the reference does not appear in the state enum
// and round-tripping is plain-data serde ===

#[test]
fn state_stores_borrowee_not_borrow() {
    let mut c = shared_borrow(5);
    assert_eq!(c.start(), CoroutineState::Yielded(5));
    let json = serde_json::to_string(&c).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let (variant, fields) = v
        .as_object()
        .expect("state serializes as an externally tagged enum")
        .iter()
        .next()
        .expect("suspended state has a variant");
    let fields = fields.as_object().expect("variant fields are a map");
    assert!(
        fields.contains_key("x"),
        "borrowee `x` must be stored in suspended variant {variant}: {json}"
    );
    assert!(
        !fields.contains_key("y"),
        "borrow `y` must not be stored in suspended variant {variant}: {json}"
    );
    assert_eq!(fields["x"], serde_json::json!(5));
}
