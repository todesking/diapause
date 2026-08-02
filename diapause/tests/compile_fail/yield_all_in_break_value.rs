#[diapause::coroutine(yield = u32)]
fn sub() -> u32 {
    yield_!(1);
    2
}

// The value of a `break` is not a tail expression: a loop produces its
// value through `break`, and delegating there is rejected even when the
// loop itself sits in a supported position.
#[diapause::coroutine(yield = u32)]
fn outer() -> u32 {
    let g: sub::State = sub();
    let v: u32 = loop {
        break yield_all!(g);
    };
    v
}

fn main() {}
