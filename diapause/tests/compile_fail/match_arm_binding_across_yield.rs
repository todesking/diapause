#[diapause::coroutine(yield = u32)]
fn f(x: Option<u32>) -> u32 {
    let mut out: u32 = 0;
    match x {
        Some(v) => {
            yield_!(1);
            out = v;
        }
        None => {}
    }
    out
}

fn main() {}
