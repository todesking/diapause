// `IntoIterator` is only generated when the resume type is `()`. A
// coroutine with a non-unit resume type has no `into_iter`, so passing it
// to a `for` loop (or calling `.into_iter()`) is a compile error.

#[diapause::coroutine(yield = u32, resume = u32)]
fn needs_resume() {
    let a: [u32; 1] = [1];
    for n in a {
        yield_!(n);
    }
}

fn main() {
    for _ in needs_resume() {}
}
