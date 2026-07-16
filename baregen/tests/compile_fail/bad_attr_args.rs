#[baregen::coroutine(banana = i32)]
fn unknown_arg() {}

#[baregen::coroutine(yield = i32, yield = u32)]
fn duplicate_arg() {}

#[baregen::coroutine(fingerprint, fingerprint)]
fn duplicate_fingerprint() {}

#[baregen::coroutine(fingerprint = 42)]
fn non_string_fingerprint() {}

fn main() {}
