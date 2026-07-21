#[diapause::coroutine(yield = i32)]
fn coro() {
    println!("{}", yield_!(1));
}

fn main() {}
