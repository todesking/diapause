#[baregen::coroutine(yield = u32)]
fn f((a, b): (u32, u32)) -> u32 {
    yield_!(a);
    b
}

fn main() {}
