#[baregen::coroutine(banana = i32)]
fn unknown_arg() {}

#[baregen::coroutine(yield = i32, yield = u32)]
fn duplicate_arg() {}

fn main() {}
