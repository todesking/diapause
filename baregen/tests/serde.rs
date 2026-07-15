//! Serializing a suspended coroutine and resuming the deserialized copy.
//!
//! The state enum stores a `for` loop's iterator with its concrete type
//! (`Range<u32>` here), so serde derives work with their ordinary
//! semantics and the mid-iteration cursor (start/end) round-trips.

use baregen::{Coroutine, CoroutineState};
use serde::{Deserialize, Serialize};

#[baregen::coroutine(yield = u32, resume = u32)]
#[derive(Serialize, Deserialize)]
fn running_sum(n: u32) -> u32 {
    let mut sum: u32 = 0;
    for i in 0u32..n {
        let w = yield_!(i);
        sum += i * w;
    }
    sum
}

#[test]
fn suspended_for_loop_round_trips_through_json() {
    let mut c = running_sum(4);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(1), CoroutineState::Yielded(1));

    // Suspended mid-iteration: the state holds the Range cursor and sum.
    let json = serde_json::to_string(&c).unwrap();
    let mut restored: running_sum::State = serde_json::from_str(&json).unwrap();

    // The original and the restored copy finish independently, with the
    // same remainder: 0*1 + 1*10 + 2*10 + 3*10 = 60.
    for c in [&mut c, &mut restored] {
        assert_eq!(c.resume(10), CoroutineState::Yielded(2));
        assert_eq!(c.resume(10), CoroutineState::Yielded(3));
        assert_eq!(c.resume(10), CoroutineState::Complete(60));
    }
}

#[test]
fn serialized_state_exposes_the_iterator_cursor() {
    let mut c = running_sum(3);
    c.start();
    let value: serde_json::Value = serde_json::to_value(&c).unwrap();
    // Suspended at the first yield: the S1 variant holds the range
    // iterator just after producing 0.
    let s1 = &value["S1"];
    assert_eq!(s1["__iter0"]["start"], 1);
    assert_eq!(s1["__iter0"]["end"], 3);
}
