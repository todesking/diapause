//! Hand-written coroutine/reference pairs for value-position forms the
//! generator emits: delegation and `?` inside value-form `if`/`match`
//! arms, value-loop bodies, and `let else` blocks. These document that
//! the macro supports each form before the generator relies on it.

// Bodies must stay identical between the coroutine and reference
// worlds, so lint-driven rewrites are not applied here.
#![allow(clippy::manual_is_multiple_of)]
#![allow(clippy::large_enum_variant)]

use diapause_difftest::{check_case, yield_};

// Sub-coroutine used as the delegation target (one yield, like a
// typical generated case).
#[diapause::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn sub_pair(a: u32) -> u32 {
    let r = yield_!(a);
    a.wrapping_add(r)
}

fn sub_pair_ref(a: u32) -> u32 {
    let r = yield_!(a);
    a.wrapping_add(r)
}

// === A/B: value-form `if` arms with delegation (let-bind + statement) ===

#[diapause::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn if_value_delegate(a0: u32, a1: u32) -> u32 {
    let x: u32 = if a0 % 2u32 == 0u32 {
        let s0: crate::sub_pair::State = crate::sub_pair(a0);
        let v0: u32 = yield_all!(s0);
        v0.wrapping_add(1u32)
    } else {
        let s1: crate::sub_pair::State = crate::sub_pair(a1);
        yield_all!(s1);
        a1
    };
    yield_!(x)
}

fn if_value_delegate_ref(a0: u32, a1: u32) -> u32 {
    let x: u32 = if a0 % 2u32 == 0u32 {
        let v0: u32 = sub_pair_ref(a0);
        v0.wrapping_add(1u32)
    } else {
        sub_pair_ref(a1);
        a1
    };
    yield_!(x)
}

#[test]
fn sanity_if_value_delegate() {
    for a0 in 0u32..4 {
        for resumes in [vec![0], vec![1, 2, 3], vec![7, 7]] {
            check_case(
                "if_value_delegate",
                &[a0, 3],
                &resumes,
                || if_value_delegate_ref(a0, 3),
                if_value_delegate(a0, 3),
            );
        }
    }
}

// === C/D: value-form `match` arms with delegation ===

#[diapause::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn match_value_delegate(a0: u32, a1: u32) -> u32 {
    let x: u32 = match a0 % 3u32 {
        0u32 => {
            let s0: crate::sub_pair::State = crate::sub_pair(a1);
            let v0: u32 = yield_all!(s0);
            v0
        }
        1u32 => {
            let s1: crate::sub_pair::State = crate::sub_pair(a0);
            yield_all!(s1);
            a0.wrapping_mul(2u32)
        }
        _ => a1,
    };
    yield_!(x)
}

fn match_value_delegate_ref(a0: u32, a1: u32) -> u32 {
    let x: u32 = match a0 % 3u32 {
        0u32 => {
            let v0: u32 = sub_pair_ref(a1);
            v0
        }
        1u32 => {
            sub_pair_ref(a0);
            a0.wrapping_mul(2u32)
        }
        _ => a1,
    };
    yield_!(x)
}

#[test]
fn sanity_match_value_delegate() {
    for a0 in 0u32..6 {
        for resumes in [vec![0], vec![1, 2, 3], vec![9]] {
            check_case(
                "match_value_delegate",
                &[a0, 5],
                &resumes,
                || match_value_delegate_ref(a0, 5),
                match_value_delegate(a0, 5),
            );
        }
    }
}

// === E: value-form `if` arm with Option `?` (yield before ? in the
// same arm; plus ? in an arm while the other arm yields) ===

#[diapause::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn if_value_try_opt(a0: u32) -> Option<u32> {
    let o0: Option<u32> = if a0 % 3u32 == 0u32 { None } else { Some(a0) };
    let x: u32 = if a0 % 2u32 == 0u32 {
        let r0 = yield_!(a0);
        let y0: u32 = o0?;
        y0.wrapping_add(r0)
    } else {
        let y1: u32 = o0?;
        y1
    };
    Some(yield_!(x))
}

fn if_value_try_opt_ref(a0: u32) -> Option<u32> {
    let o0: Option<u32> = if a0 % 3u32 == 0u32 { None } else { Some(a0) };
    let x: u32 = if a0 % 2u32 == 0u32 {
        let r0 = yield_!(a0);
        let y0: u32 = o0?;
        y0.wrapping_add(r0)
    } else {
        let y1: u32 = o0?;
        y1
    };
    Some(yield_!(x))
}

#[test]
fn sanity_if_value_try_opt() {
    for a0 in 0u32..7 {
        for resumes in [vec![0], vec![1, 2, 3]] {
            check_case(
                "if_value_try_opt",
                &[a0],
                &resumes,
                || if_value_try_opt_ref(a0),
                if_value_try_opt(a0),
            );
        }
    }
}

// === F: value-form `match` arm with Result `?` (Err2 -> Err1 From
// conversion) ===

#[diapause::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn match_value_try_res(a0: u32) -> Result<u32, diapause_difftest::Err1> {
    let q0: Result<u32, diapause_difftest::Err2> = if a0 % 3u32 == 0u32 {
        Err(diapause_difftest::Err2(a0))
    } else {
        Ok(a0)
    };
    let x: u32 = match a0 % 2u32 {
        0u32 => {
            let r0 = yield_!(a0);
            let y0: u32 = q0?;
            y0.wrapping_add(r0)
        }
        _ => {
            let y1: u32 = q0?;
            y1
        }
    };
    Ok(yield_!(x))
}

fn match_value_try_res_ref(a0: u32) -> Result<u32, diapause_difftest::Err1> {
    let q0: Result<u32, diapause_difftest::Err2> = if a0 % 3u32 == 0u32 {
        Err(diapause_difftest::Err2(a0))
    } else {
        Ok(a0)
    };
    let x: u32 = match a0 % 2u32 {
        0u32 => {
            let r0 = yield_!(a0);
            let y0: u32 = q0?;
            y0.wrapping_add(r0)
        }
        _ => {
            let y1: u32 = q0?;
            y1
        }
    };
    Ok(yield_!(x))
}

#[test]
fn sanity_match_value_try_res() {
    for a0 in 0u32..7 {
        for resumes in [vec![0], vec![4, 5]] {
            check_case(
                "match_value_try_res",
                &[a0],
                &resumes,
                || match_value_try_res_ref(a0),
                match_value_try_res(a0),
            );
        }
    }
}

// === G: value-loop body with delegation ===

#[diapause::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn value_loop_delegate(a0: u32) -> u32 {
    let mut c0: u32 = 0u32;
    let x: u32 = 'l0: loop {
        if c0 >= 2u32 {
            break 'l0 a0;
        }
        c0 = c0.wrapping_add(1u32);
        let s0: crate::sub_pair::State = crate::sub_pair(c0);
        let v0: u32 = yield_all!(s0);
        if v0 % 2u32 == 0u32 {
            break 'l0 v0;
        }
    };
    yield_!(x)
}

fn value_loop_delegate_ref(a0: u32) -> u32 {
    let mut c0: u32 = 0u32;
    let x: u32 = 'l0: loop {
        if c0 >= 2u32 {
            break 'l0 a0;
        }
        c0 = c0.wrapping_add(1u32);
        let v0: u32 = sub_pair_ref(c0);
        if v0 % 2u32 == 0u32 {
            break 'l0 v0;
        }
    };
    yield_!(x)
}

#[test]
fn sanity_value_loop_delegate() {
    for a0 in 0u32..4 {
        for resumes in [vec![0], vec![1, 2, 3], vec![5, 8]] {
            check_case(
                "value_loop_delegate",
                &[a0],
                &resumes,
                || value_loop_delegate_ref(a0),
                value_loop_delegate(a0),
            );
        }
    }
}

// === H: value-loop body with Option `?` ===

#[diapause::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn value_loop_try(a0: u32) -> Option<u32> {
    let o0: Option<u32> = if a0 % 3u32 == 0u32 { None } else { Some(a0) };
    let mut c0: u32 = 0u32;
    let x: u32 = 'l0: loop {
        if c0 >= 2u32 {
            break 'l0 a0;
        }
        c0 = c0.wrapping_add(1u32);
        let r0 = yield_!(c0);
        let y0: u32 = o0?;
        if y0.wrapping_add(r0) % 2u32 == 0u32 {
            break 'l0 y0;
        }
    };
    Some(yield_!(x))
}

fn value_loop_try_ref(a0: u32) -> Option<u32> {
    let o0: Option<u32> = if a0 % 3u32 == 0u32 { None } else { Some(a0) };
    let mut c0: u32 = 0u32;
    let x: u32 = 'l0: loop {
        if c0 >= 2u32 {
            break 'l0 a0;
        }
        c0 = c0.wrapping_add(1u32);
        let r0 = yield_!(c0);
        let y0: u32 = o0?;
        if y0.wrapping_add(r0) % 2u32 == 0u32 {
            break 'l0 y0;
        }
    };
    Some(yield_!(x))
}

#[test]
fn sanity_value_loop_try() {
    for a0 in 0u32..7 {
        for resumes in [vec![0], vec![1, 2, 3]] {
            check_case(
                "value_loop_try",
                &[a0],
                &resumes,
                || value_loop_try_ref(a0),
                value_loop_try(a0),
            );
        }
    }
}

// === I: `let ... else` block with delegation (let-bind + statement),
// diverging via `return` ===

#[diapause::coroutine(yield = u32, resume = u32)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
fn let_else_delegate(a0: u32) -> u32 {
    let 0u32 = a0 % 2u32 else {
        let s0: crate::sub_pair::State = crate::sub_pair(a0);
        let v0: u32 = yield_all!(s0);
        let s1: crate::sub_pair::State = crate::sub_pair(v0);
        yield_all!(s1);
        return v0;
    };
    yield_!(a0)
}

fn let_else_delegate_ref(a0: u32) -> u32 {
    let 0u32 = a0 % 2u32 else {
        let v0: u32 = sub_pair_ref(a0);
        sub_pair_ref(v0);
        return v0;
    };
    yield_!(a0)
}

#[test]
fn sanity_let_else_delegate() {
    for a0 in 0u32..4 {
        for resumes in [vec![0], vec![1, 2, 3], vec![6]] {
            check_case(
                "let_else_delegate",
                &[a0],
                &resumes,
                || let_else_delegate_ref(a0),
                let_else_delegate(a0),
            );
        }
    }
}
