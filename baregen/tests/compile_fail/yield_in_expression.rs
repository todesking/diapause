fn f(_: i32, _: i32) {}

#[baregen::coroutine(yield = i32, resume = i32)]
fn coro() {
    f(1, yield_!(2));
    let _x = 1 + yield_!(3);
}

fn main() {}
