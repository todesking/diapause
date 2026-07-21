#[diapause::coroutine(yield = u32)]
fn shadowed_source() -> u32 {
    let x: u32 = 1;
    let y = &x;
    yield_!(0);
    let x: u32 = 99;
    let _ = x;
    yield_!(0);
    *y
}

fn main() {}
