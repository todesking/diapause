#[baregen::coroutine(yield = i32)]
fn value_position(c: bool) -> i32 {
    let x = if c {
        yield_!(1);
        1
    } else {
        2
    };
    x
}

fn main() {}
