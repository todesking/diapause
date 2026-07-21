#[diapause::coroutine(yield = u32)]
fn f(x: Option<u32>) -> u32 {
    let Some(v) = x else {
        yield_!(0);
        return 0;
    };
    yield_!(1);
    v
}

fn main() {}
