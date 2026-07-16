#[baregen::coroutine(yield = u32)]
fn sub() -> u32 {
    yield_!(1);
    2
}

#[baregen::coroutine(yield = u32)]
fn outer() -> u32 {
    let v: u32 = yield_all!(sub());
    v
}

fn main() {}
