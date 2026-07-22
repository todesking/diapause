//! Baseline binary for code-size comparison: same I/O scaffolding as
//! the other `size_*` examples but no coroutine machinery. Subtract
//! this binary's size from the others to estimate each approach's code
//! footprint.

fn main() {
    let n: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    println!("{}", n);
    println!("{}", n.wrapping_mul(3));
    println!("{}", n ^ 0x55);
}
