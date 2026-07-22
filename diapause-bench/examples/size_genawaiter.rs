//! Code-size probe: the three workloads as genawaiter `rc::Gen`
//! generators. Compare the release binary size against `size_baseline`.

use diapause_bench::{drive_ga, drive_ga_total, ga};

fn main() {
    let n: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    println!("{}", drive_ga(ga::counter(n)));
    println!("{}", drive_ga(ga::nested(n)));
    println!("{}", drive_ga_total(ga::running_total(n)));
}
