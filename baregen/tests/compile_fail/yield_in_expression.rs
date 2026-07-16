fn f(_: i32, _: i32) {}
fn g() -> i32 {
    0
}

// A call evaluated before the yield_! ends the hoistable prefix: moving
// the yield in front of the statement would reorder it across `g()`.
#[baregen::coroutine(yield = i32, resume = i32)]
fn coro() {
    f(g(), yield_!(2));
    let _x = g() + yield_!(3);
}

fn main() {}
