#[baregen::coroutine(yield = i32)]
fn in_if(cond: bool) {
    if cond {
        yield_!(1);
    }
}

#[baregen::coroutine(yield = i32)]
fn in_loop() {
    loop {
        yield_!(1);
    }
}

#[baregen::coroutine(yield = i32)]
fn in_match(x: i32) {
    match x {
        0 => {
            yield_!(1);
        }
        _ => {}
    }
}

#[baregen::coroutine(yield = i32)]
fn in_while(cond: bool) {
    while cond {
        yield_!(1);
    }
}

#[baregen::coroutine(yield = i32)]
fn in_for() {
    for _i in 0..3 {
        yield_!(1);
    }
}

fn main() {}
