#[baregen::coroutine(yield = i32)]
fn in_for() {
    for _i in 0..3 {
        yield_!(1);
    }
}

fn main() {}
