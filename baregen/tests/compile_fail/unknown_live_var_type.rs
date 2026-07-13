fn compute() -> Vec<u32> {
    vec![]
}

#[baregen::coroutine(yield = i32)]
fn coro() -> usize {
    let x = compute();
    let unsuffixed = 1;
    yield_!(1);
    x.len() + unsuffixed
}

fn main() {}
