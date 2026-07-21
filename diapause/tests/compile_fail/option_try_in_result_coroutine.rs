#[diapause::coroutine(yield = u32)]
fn mixed(x: Option<u32>) -> Result<u32, String> {
    let v: u32 = x?;
    yield_!(v);
    Ok(v)
}

fn main() {}
