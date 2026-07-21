#[diapause::coroutine(yield = i32, resume = (i32, i32))]
fn destructuring_binding() {
    let (a, b) = yield_!(1);
    let _ = (a, b);
}

fn main() {}
