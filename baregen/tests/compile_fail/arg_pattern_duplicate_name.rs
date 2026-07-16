#[baregen::coroutine(yield = u32)]
fn f(a: u32, (a, b): (u32, u32)) -> u32 {
    yield_!(b);
    a
}

fn main() {}
