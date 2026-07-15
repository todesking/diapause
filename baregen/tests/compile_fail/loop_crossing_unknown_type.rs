fn compute() -> i32 {
    0
}

#[baregen::coroutine(yield = i32)]
fn unknown(n: i32) {
    let mut acc = compute();
    while acc < n {
        yield_!(1);
        acc += 1;
    }
}

fn main() {}
