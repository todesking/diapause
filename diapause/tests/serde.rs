//! Serializing a suspended coroutine and resuming the deserialized copy.
//!
//! The state enum stores a `for` loop's iterator with its concrete type
//! (`Range<u32>` here), so serde derives work with their ordinary
//! semantics and the mid-iteration cursor (start/end) round-trips.

use diapause::{Coroutine, CoroutineState};
use serde::{Deserialize, Serialize};

#[diapause::coroutine(yield = u32, resume = u32)]
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

// Two "versions" of one coroutine, both with the `fingerprint` flag:
// the state layouts are identical (same variant and field names/types),
// so JSON persisted by one deserializes structurally into the other —
// reproducing a persisted state meeting edited source. tally_v1's body
// is also identical to running_sum's (which lacks the flag), for the
// no-fingerprint -> fingerprint migration test.

#[diapause::coroutine(yield = u32, resume = u32, fingerprint)]
#[derive(Serialize, Deserialize)]
fn tally_v1(n: u32) -> u32 {
    let mut sum: u32 = 0;
    for i in 0u32..n {
        let w = yield_!(i);
        sum += i * w;
    }
    sum
}

#[diapause::coroutine(yield = u32, resume = u32, fingerprint)]
#[derive(Serialize, Deserialize)]
fn tally_v2(n: u32) -> u32 {
    let mut sum: u32 = 0;
    for i in 0u32..n {
        let w = yield_!(i);
        sum += i + w;
    }
    sum
}

#[test]
fn fingerprinted_state_round_trips_through_json() {
    let mut c = tally_v1(4);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(1), CoroutineState::Yielded(1));

    let json = serde_json::to_string(&c).unwrap();
    let mut restored: tally_v1::State = serde_json::from_str(&json).unwrap();
    restored.check_fingerprint().unwrap();

    for c in [&mut c, &mut restored] {
        assert_eq!(c.resume(10), CoroutineState::Yielded(2));
        assert_eq!(c.resume(10), CoroutineState::Yielded(3));
        assert_eq!(c.resume(10), CoroutineState::Complete(60));
    }
}

#[test]
fn fingerprints_differ_between_edited_sources() {
    assert_ne!(tally_v1::State::FINGERPRINT, tally_v2::State::FINGERPRINT);
}

#[test]
fn check_fingerprint_rejects_a_state_from_edited_source() {
    let mut c = tally_v1(4);
    c.start();
    let json = serde_json::to_string(&c).unwrap();

    // Structurally compatible, so deserialization itself succeeds; the
    // fingerprint is what detects the version skew.
    let restored: tally_v2::State = serde_json::from_str(&json).unwrap();
    let err = restored.check_fingerprint().unwrap_err();
    assert_eq!(err.expected, tally_v2::State::FINGERPRINT);
    assert_eq!(err.found, tally_v1::State::FINGERPRINT);

    let err: &dyn std::error::Error = &err;
    assert!(err.to_string().contains("fingerprint mismatch"));
}

#[test]
#[should_panic(expected = "this state was created by a different version of `tally_v2`")]
fn resume_panics_on_a_state_from_edited_source() {
    let mut c = tally_v1(4);
    c.start();
    let json = serde_json::to_string(&c).unwrap();
    let mut restored: tally_v2::State = serde_json::from_str(&json).unwrap();
    restored.resume(1);
}

#[test]
#[should_panic(expected = "this state was created by a different version of `tally_v2`")]
fn start_panics_on_a_state_from_edited_source() {
    // A fresh state suspends nothing yet Start already carries the
    // fingerprint, so even an unstarted persisted state is checked.
    let c = tally_v1(4);
    let json = serde_json::to_string(&c).unwrap();
    let mut restored: tally_v2::State = serde_json::from_str(&json).unwrap();
    restored.start();
}

#[test]
fn enabling_fingerprint_invalidates_old_persisted_states() {
    // running_sum has the same body but no `fingerprint` flag: its
    // states lack the `__fp` field, so pre-fingerprint data fails to
    // deserialize (missing field) instead of resuming unchecked.
    let mut c = running_sum(4);
    c.start();
    let json = serde_json::to_string(&c).unwrap();
    assert!(serde_json::from_str::<tally_v1::State>(&json).is_err());
}

// A manual `fingerprint = "tag"` override pins the fingerprint across
// edits: the user asserts that states persisted under the same tag stay
// resumable (the state layout must actually still match).

#[diapause::coroutine(yield = u32, resume = u32, fingerprint = "tally-pin")]
#[derive(Serialize, Deserialize)]
fn pinned_v1(n: u32) -> u32 {
    let mut sum: u32 = 0;
    for i in 0u32..n {
        let w = yield_!(i);
        sum += i * w;
    }
    sum
}

#[diapause::coroutine(yield = u32, resume = u32, fingerprint = "tally-pin")]
#[derive(Serialize, Deserialize)]
fn pinned_v2(n: u32) -> u32 {
    let mut sum: u32 = 0;
    for i in 0u32..n {
        let w = yield_!(i);
        sum += i + w;
    }
    sum
}

#[test]
fn manual_fingerprint_pins_compatibility_across_edits() {
    assert_eq!(pinned_v1::State::FINGERPRINT, pinned_v2::State::FINGERPRINT);

    let mut c = pinned_v1(4);
    c.start();
    let json = serde_json::to_string(&c).unwrap();

    // The v1 state resumes under v2's edited body without complaint.
    let mut restored: pinned_v2::State = serde_json::from_str(&json).unwrap();
    restored.check_fingerprint().unwrap();
    assert_eq!(restored.resume(10), CoroutineState::Yielded(1));
}

#[test]
fn fingerprint_const_is_generated_without_the_flag() {
    // `FINGERPRINT` exists on every state enum, `fingerprint` flag or
    // not, for users who manage compatibility themselves.
    let fp: u64 = running_sum::State::FINGERPRINT;
    assert_eq!(fp, running_sum::State::FINGERPRINT);
}

// `yield_all!` stores the inner coroutine's state enum by value in the
// outer one, so serde derives compose across the nesting: a coroutine
// suspended inside a delegation serializes with the inner state included.

#[diapause::coroutine(yield = u32, resume = u32)]
#[derive(Serialize, Deserialize)]
fn sub_sum(n: u32) -> u32 {
    let mut sum: u32 = 0;
    for i in 0u32..n {
        let w = yield_!(i);
        sum += w;
    }
    sum
}

#[diapause::coroutine(yield = u32, resume = u32)]
#[derive(Serialize, Deserialize)]
fn delegating(n: u32) -> u32 {
    let g: sub_sum::State = sub_sum(n);
    let total: u32 = yield_all!(g);
    total * 2
}

#[test]
fn suspended_delegation_round_trips_through_json() {
    let mut c = delegating(3);
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(10), CoroutineState::Yielded(1));

    // Suspended mid-delegation: the outer state holds the inner State
    // (itself suspended mid-iteration) as an ordinary nested value.
    let value: serde_json::Value = serde_json::to_value(&c).unwrap();
    let inner = &value["S2"]["__dg0"];
    assert!(inner.is_object(), "inner state missing: {value}");

    let json = serde_json::to_string(&c).unwrap();
    let mut restored: delegating::State = serde_json::from_str(&json).unwrap();

    // Both copies finish independently with the same tally:
    // (10 + 20 + 30) * 2 = 120.
    for c in [&mut c, &mut restored] {
        assert_eq!(c.resume(20), CoroutineState::Yielded(2));
        assert_eq!(c.resume(30), CoroutineState::Complete(120));
    }
}

// `a..=b` iterators are stored as the generated `__RangeInclusiveIter`
// (start/end/done): serde's `RangeInclusive` impl serializes only
// `start`/`end` and drops the internal exhaustion flag, so storing the
// std type would make a state saved after the final element re-yield
// that element forever after a round trip.

#[diapause::coroutine(yield = u32)]
#[derive(Serialize, Deserialize)]
fn inclusive_sum(n: u32) -> u32 {
    let mut sum: u32 = 0;
    for i in 0u32..=n {
        yield_!(i);
        sum += i;
    }
    sum
}

#[test]
fn inclusive_range_round_trips_at_every_suspension() {
    // Continue on the round-tripped copy after every yield; the state
    // suspended after the final element (exhausted iterator) must
    // complete instead of re-yielding it.
    let mut c = inclusive_sum(2);
    let mut step = c.start();
    let mut yields = Vec::new();
    while let CoroutineState::Yielded(v) = step {
        yields.push(v);
        let json = serde_json::to_string(&c).unwrap();
        c = serde_json::from_str(&json).unwrap();
        step = c.resume(());
    }
    assert_eq!(yields, [0, 1, 2]);
    assert_eq!(step, CoroutineState::Complete(3));
}

#[test]
fn serialized_inclusive_iterator_exposes_exhaustion() {
    let mut c = inclusive_sum(1);
    c.start();
    assert_eq!(c.resume(()), CoroutineState::Yielded(1));

    // Suspended after the final element: the wrapper records the
    // exhaustion explicitly instead of relying on `RangeInclusive`'s
    // unserialized internal flag.
    let value: serde_json::Value = serde_json::to_value(&c).unwrap();
    let it = &value["S1"]["__iter0"];
    assert_eq!(it["start"], 1);
    assert_eq!(it["end"], 1);
    assert_eq!(it["done"], true);
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
