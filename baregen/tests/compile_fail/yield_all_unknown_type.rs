#[baregen::coroutine(yield = u32)]
fn sub() -> u32 {
    yield_!(1);
    2
}

#[baregen::coroutine(yield = u32)]
fn outer() -> u32 {
    let g = sub();
    let v: u32 = yield_all!(g);
    v
}

fn main() {}
