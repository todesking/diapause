#[diapause::coroutine(yield = i32)]
fn missing_annotation(c: bool) -> i32 {
    let x = if c {
        yield_!(1);
        1
    } else {
        2
    };
    x
}

fn main() {}
