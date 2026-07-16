fn f() {}
fn g() -> bool {
    true
}
fn h(_: bool, _: i32) -> i32 {
    0
}

// The right operand of `&&` is conditionally evaluated, so the yield_!
// cannot be hoisted out of the condition.
#[baregen::coroutine(yield = i32, resume = bool)]
fn in_if_cond() {
    if g() && yield_!(1) {
        f();
    }
}

// A `while` condition is re-evaluated every iteration; hoisting is
// never possible.
#[baregen::coroutine(yield = i32, resume = bool)]
fn in_while_cond() {
    while yield_!(2) {
        f();
    }
}

// `g()` runs before the yield_!, so the scrutinee is not hoistable.
#[baregen::coroutine(yield = i32, resume = i32)]
fn in_scrutinee() {
    match h(g(), yield_!(3)) {
        _ => f(),
    }
}

fn main() {}
