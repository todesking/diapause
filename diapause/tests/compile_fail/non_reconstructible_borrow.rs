struct Wrap {
    field: u32,
}

#[diapause::coroutine(yield = i32)]
fn complex_borrow(w: Wrap) -> u32 {
    let y = &w.field;
    yield_!(1);
    *y
}

#[diapause::coroutine(yield = i32)]
fn reference_holding_value(v: Vec<u32>) -> u32 {
    let first: &u32 = v.first().unwrap();
    yield_!(1);
    *first
}

fn main() {}
