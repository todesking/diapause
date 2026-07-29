#[diapause::coroutine(yield = u32, resume = u32)]
fn sub(n: u32) -> u32 {
    let a = yield_!(n);
    n + a
}

#[diapause::coroutine(yield = u32, resume = u32)]
fn outer(g: sub::State) -> u32 {
    let v: u32 = yield_all_resume!(g);
    v
}

fn main() {}
