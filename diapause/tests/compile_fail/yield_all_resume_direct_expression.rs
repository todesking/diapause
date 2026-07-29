#[diapause::coroutine(yield = u32, resume = u32)]
fn sub(n: u32) -> u32 {
    let a = yield_!(n);
    n + a
}

#[diapause::coroutine(yield = u32, resume = u32)]
fn outer() -> u32 {
    let v: u32 = yield_all_resume!(sub(1), 0);
    v
}

fn main() {}
