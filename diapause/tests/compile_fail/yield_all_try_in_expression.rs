#[diapause::coroutine(yield = u32)]
fn sub() -> Result<u32, ()> {
    yield_!(1);
    Ok(2)
}

fn f(v: u32) -> u32 {
    v
}

// `?` on a delegation is only supported where the delegation itself is:
// inside a larger expression it is rejected like the bare macro.
#[diapause::coroutine(yield = u32)]
fn outer() -> Result<u32, ()> {
    let g: sub::State = sub();
    f(yield_all!(g)?);
    yield_!(1);
    Ok(1)
}

fn main() {}
