#[diapause::coroutine(yield = u32)]
fn f(opt: Option<u32>, c: bool) -> u32 {
    if let Some(x) = opt && c {
        yield_!(x);
    }
    0
}

fn main() {}
