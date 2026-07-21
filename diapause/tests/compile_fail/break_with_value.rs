#[diapause::coroutine(yield = i32)]
fn break_value() {
    loop {
        yield_!(1);
        break 42;
    }
    after();
}

fn after() {}

fn main() {}
