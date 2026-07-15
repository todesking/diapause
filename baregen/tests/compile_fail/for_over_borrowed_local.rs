#[baregen::coroutine(yield = u32)]
fn borrows_local() {
    let v: [u32; 3] = [1, 2, 3];
    for x in &v {
        yield_!(*x);
    }
}

fn main() {}
