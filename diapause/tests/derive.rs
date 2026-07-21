//! `#[derive(...)]` below the coroutine attribute is moved onto the
//! generated State enum.

#![deny(warnings)]

use diapause::{Coroutine, CoroutineState};

#[diapause::coroutine(yield = i32, resume = i32)]
#[derive(Clone)]
fn accumulate(start: i32) -> i32 {
    let a = yield_!(start);
    let b = yield_!(start + a);
    start + a + b
}

#[test]
fn cloned_snapshot_resumes_independently() {
    let mut c = accumulate(10);
    assert_eq!(c.start(), CoroutineState::Yielded(10));
    assert_eq!(c.resume(1), CoroutineState::Yielded(11));
    let mut snapshot = c.clone();
    assert_eq!(c.resume(2), CoroutineState::Complete(13));
    assert_eq!(snapshot.resume(100), CoroutineState::Complete(111));
}

#[diapause::coroutine(yield = u32)]
#[derive(Clone, Debug)]
fn pair<T: Clone>(x: T) -> (T, T) {
    yield_!(1);
    let y = x.clone();
    (x, y)
}

#[test]
fn generic_clone_gets_standard_bounds() {
    let mut c = pair(String::from("v"));
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    let mut snapshot = c.clone();
    assert_eq!(
        c.resume(()),
        CoroutineState::Complete(("v".to_string(), "v".to_string()))
    );
    assert_eq!(
        snapshot.resume(()),
        CoroutineState::Complete(("v".to_string(), "v".to_string()))
    );
}

#[test]
fn multiple_derives_including_debug() {
    let c = pair(1u8);
    assert!(format!("{c:?}").contains("Start"));
}

/// A documented coroutine: the doc comment stays on the starter fn.
///
/// This test file denies warnings, so misplaced attributes would fail.
#[diapause::coroutine]
#[allow(clippy::let_and_return)]
fn documented(x: u8) -> u8 {
    x
}

#[test]
fn doc_comments_build_cleanly() {
    let mut c = documented(3);
    assert_eq!(c.start(), CoroutineState::Complete(3));
}
