//! Code-size probe: the three workloads as handwritten state machines.
//! Compare the release binary size against `size_baseline`.

use diapause_bench::{drive, drive_total, hand};

fn main() {
    let n: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    println!("{}", drive(hand::counter(n)));
    println!("{}", drive(hand::nested(n)));
    println!("{}", drive_total(hand::running_total(n)));
}
