#[baregen::coroutine(yield = u32)]
fn destructured(sink: fn(u32)) {
    let pairs: [(u32, u32); 2] = [(1, 2), (3, 4)];
    for (a, b) in pairs {
        yield_!(a);
        sink(b);
    }
}

fn main() {}
