//! Code-size probe: the three workloads as diapause coroutines.
//! Compare the release binary size against `size_baseline`.

use diapause_bench::{dia, drive, drive_total};

fn main() {
    let n: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    println!("{}", drive(dia::counter(n)));
    println!("{}", drive(dia::nested(n)));
    println!("{}", drive_total(dia::running_total(n)));
}
