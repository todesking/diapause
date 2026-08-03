// The state of a coroutine taking a reference is generic over its
// lifetime, which a call operand cannot spell: `sub::State` is stored in
// the outer state, where an elided lifetime is not allowed. Bind the
// coroutine first with an annotation naming the lifetime instead.
#[diapause::coroutine(yield = u32)]
fn sub(x: &u32) -> u32 {
    yield_!(*x);
    *x + 1
}

#[diapause::coroutine(yield = u32)]
fn outer(x: &u32) -> u32 {
    let v: u32 = yield_all!(sub(x));
    v
}

fn main() {}
