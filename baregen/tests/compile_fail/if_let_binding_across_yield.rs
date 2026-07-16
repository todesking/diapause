#[baregen::coroutine(yield = u32)]
fn f(x: Option<u32>) -> u32 {
    let mut out: u32 = 0;
    if let Some(v) = x {
        yield_!(1);
        out = v;
    }
    out
}

fn main() {}
