#[diapause::coroutine(yield = u32)]
fn sub() -> u32 {
    yield_!(1);
    2
}

#[diapause::coroutine(yield = u32)]
fn outer() -> u32 {
    let v: u32 = yield_all!(box sub());
    v
}

fn main() {}
