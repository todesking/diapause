// A call operand derives the delegate's type from the callee path, so
// a function that is not a coroutine leaves `make::State` unresolved.
fn make() -> u32 {
    0
}

#[diapause::coroutine(yield = u32)]
fn outer() -> u32 {
    let v: u32 = yield_all!(make());
    v
}

fn main() {}
