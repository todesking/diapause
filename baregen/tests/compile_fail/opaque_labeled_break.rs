#[baregen::coroutine(yield = i32)]
fn labeled(c: bool) {
    'outer: loop {
        yield_!(1);
        if c {
            break 'outer;
        }
    }
}

#[baregen::coroutine(yield = i32)]
fn unlabeled(c: bool) {
    loop {
        yield_!(1);
        if c {
            break;
        }
    }
}

fn main() {}
