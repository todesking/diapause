fn f() {}

#[baregen::coroutine(yield = i32, resume = bool)]
fn in_if_cond() {
    if yield_!(1) {
        f();
    }
}

#[baregen::coroutine(yield = i32, resume = bool)]
fn in_while_cond() {
    while yield_!(2) {
        f();
    }
}

#[baregen::coroutine(yield = i32, resume = i32)]
fn in_scrutinee() {
    match yield_!(3) {
        _ => f(),
    }
}

fn main() {}
