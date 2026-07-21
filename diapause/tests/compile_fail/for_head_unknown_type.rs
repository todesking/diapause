fn items() -> Vec<u32> {
    vec![1, 2, 3]
}

#[diapause::coroutine(yield = u32)]
fn unknown_head() {
    for x in items() {
        yield_!(x);
    }
}

fn main() {}
