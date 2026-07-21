#[diapause::coroutine(banana = i32)]
fn unknown_arg() {}

#[diapause::coroutine(yield = i32, yield = u32)]
fn duplicate_arg() {}

#[diapause::coroutine(fingerprint, fingerprint)]
fn duplicate_fingerprint() {}

#[diapause::coroutine(fingerprint = 42)]
fn non_string_fingerprint() {}

fn main() {}
