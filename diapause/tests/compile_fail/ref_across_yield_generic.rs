// Holding a borrowed reference across a yield in a generic context
// should be rejected when the borrow cannot be proven to live across the yield.
#[diapause::coroutine(yield = u32)]
fn generic_borrowed_across_yield<T>(items: &[T]) -> usize {
    let reference = &items[0];
    yield_!(1);
    // Using the reference after the yield should fail because the coroutine
    // state cannot safely store a reference.
    let _x = std::ptr::eq(reference, reference);
    items.len()
}

fn main() {}
