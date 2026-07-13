//! Task 01 smoke test: the attribute can be applied via `baregen::coroutine`.
//! The macro is still a passthrough, so the function behaves as written.

#[baregen::coroutine]
fn plain(a: u32, b: u32) -> u32 {
    a + b
}

#[test]
fn attribute_applies() {
    assert_eq!(plain(2, 3), 5);
}
