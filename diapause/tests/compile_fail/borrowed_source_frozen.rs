use diapause::Coroutine;

#[diapause::coroutine(yield = i32)]
fn fill(v: &mut Vec<i32>) {
    v.push(1);
    yield_!(1);
    v.push(2);
}

fn main() {
    let mut v = Vec::new();
    let mut c = fill(&mut v);
    c.start();
    // The state still holds &mut v, so this must be rejected.
    v.push(99);
    c.resume(());
}
