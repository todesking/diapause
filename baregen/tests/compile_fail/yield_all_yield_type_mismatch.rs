#[baregen::coroutine(yield = String)]
fn words() -> u32 {
    yield_!(String::from("a"));
    1
}

#[baregen::coroutine(yield = u32)]
fn outer() -> u32 {
    let g: words::State = words();
    let v: u32 = yield_all!(g);
    v
}

fn main() {}
