//! Benchmark workloads comparing `#[diapause::coroutine]`-generated
//! state machines against genawaiter generators and handwritten state
//! machines.
//!
//! Each workload exists in three implementations with identical
//! observable behavior (verified by the tests at the bottom):
//!
//! - [`dia`](self::dia): `#[diapause::coroutine]`-generated state
//!   machines
//! - [`hand`](self::hand): handwritten state machines implementing
//!   [`diapause::Coroutine`]
//! - [`ga`](self::ga): genawaiter (`rc::Gen`; the `stack::Gen` variants
//!   are built inline in the benches because they cannot be returned by
//!   value)
//!
//! Workloads:
//!
//! - `counter(n)`: yields `0..n` — the minimal resume/yield round trip.
//! - `nested(n)`: two nested loops, yields `i ^ j` for `j < i < n` —
//!   suspension points under nested control flow.
//! - `running_total(n)`: yields a running sum and folds the resume
//!   argument back in — exercises resume values and a completion value.

use diapause::{Coroutine, CoroutineState, CoroutineStatus};

/// `#[diapause::coroutine]` implementations.
pub mod dia {
    #[diapause::coroutine(yield = u64)]
    pub fn counter(n: u64) {
        for i in 0..n {
            yield_!(i);
        }
    }

    #[diapause::coroutine(yield = u64)]
    pub fn nested(n: u64) {
        for i in 0..n {
            for j in 0..i {
                yield_!(i ^ j);
            }
        }
    }

    #[diapause::coroutine(yield = u64, resume = u64)]
    pub fn running_total(n: u64) -> u64 {
        let mut sum: u64 = 0;
        for i in 0..n {
            let bonus = yield_!(sum);
            sum = sum.wrapping_add(i).wrapping_add(bonus);
        }
        sum
    }
}

/// genawaiter implementations (heap-allocating `rc::Gen`).
pub mod ga {
    use std::future::Future;

    use genawaiter::rc::Gen;

    pub fn counter(n: u64) -> Gen<u64, (), impl Future<Output = ()>> {
        Gen::new(move |co| async move {
            for i in 0..n {
                co.yield_(i).await;
            }
        })
    }

    pub fn nested(n: u64) -> Gen<u64, (), impl Future<Output = ()>> {
        Gen::new(move |co| async move {
            for i in 0..n {
                for j in 0..i {
                    co.yield_(i ^ j).await;
                }
            }
        })
    }

    pub fn running_total(n: u64) -> Gen<u64, u64, impl Future<Output = u64>> {
        Gen::new(move |co| async move {
            let mut sum: u64 = 0;
            for i in 0..n {
                let bonus = co.yield_(sum).await;
                sum = sum.wrapping_add(i).wrapping_add(bonus);
            }
            sum
        })
    }
}

/// Handwritten state machines implementing [`diapause::Coroutine`],
/// written the way one would by hand: a struct of live variables plus a
/// resume method, with the same status bookkeeping the generated code
/// performs.
pub mod hand {
    use super::*;

    pub struct Counter {
        n: u64,
        next: u64,
        status: CoroutineStatus,
    }

    pub fn counter(n: u64) -> Counter {
        Counter {
            n,
            next: 0,
            status: CoroutineStatus::NotStarted,
        }
    }

    impl Counter {
        fn step(&mut self) -> CoroutineState<u64, ()> {
            if self.next < self.n {
                let v = self.next;
                self.next += 1;
                self.status = CoroutineStatus::Suspended;
                CoroutineState::Yielded(v)
            } else {
                self.status = CoroutineStatus::Done;
                CoroutineState::Complete(())
            }
        }
    }

    impl Coroutine for Counter {
        type Yield = u64;
        type Return = ();

        fn start(&mut self) -> CoroutineState<u64, ()> {
            assert_eq!(self.status, CoroutineStatus::NotStarted);
            self.step()
        }

        fn resume(&mut self, (): ()) -> CoroutineState<u64, ()> {
            assert_eq!(self.status, CoroutineStatus::Suspended);
            self.step()
        }

        fn status(&self) -> CoroutineStatus {
            self.status
        }
    }

    pub struct Nested {
        n: u64,
        i: u64,
        j: u64,
        status: CoroutineStatus,
    }

    pub fn nested(n: u64) -> Nested {
        Nested {
            n,
            i: 0,
            j: 0,
            status: CoroutineStatus::NotStarted,
        }
    }

    impl Nested {
        fn step(&mut self) -> CoroutineState<u64, ()> {
            while self.i < self.n {
                if self.j < self.i {
                    let v = self.i ^ self.j;
                    self.j += 1;
                    self.status = CoroutineStatus::Suspended;
                    return CoroutineState::Yielded(v);
                }
                self.i += 1;
                self.j = 0;
            }
            self.status = CoroutineStatus::Done;
            CoroutineState::Complete(())
        }
    }

    impl Coroutine for Nested {
        type Yield = u64;
        type Return = ();

        fn start(&mut self) -> CoroutineState<u64, ()> {
            assert_eq!(self.status, CoroutineStatus::NotStarted);
            self.step()
        }

        fn resume(&mut self, (): ()) -> CoroutineState<u64, ()> {
            assert_eq!(self.status, CoroutineStatus::Suspended);
            self.step()
        }

        fn status(&self) -> CoroutineStatus {
            self.status
        }
    }

    pub struct RunningTotal {
        n: u64,
        i: u64,
        sum: u64,
        status: CoroutineStatus,
    }

    pub fn running_total(n: u64) -> RunningTotal {
        RunningTotal {
            n,
            i: 0,
            sum: 0,
            status: CoroutineStatus::NotStarted,
        }
    }

    impl Coroutine<u64> for RunningTotal {
        type Yield = u64;
        type Return = u64;

        fn start(&mut self) -> CoroutineState<u64, u64> {
            assert_eq!(self.status, CoroutineStatus::NotStarted);
            if self.i < self.n {
                self.status = CoroutineStatus::Suspended;
                CoroutineState::Yielded(self.sum)
            } else {
                self.status = CoroutineStatus::Done;
                CoroutineState::Complete(self.sum)
            }
        }

        fn resume(&mut self, bonus: u64) -> CoroutineState<u64, u64> {
            assert_eq!(self.status, CoroutineStatus::Suspended);
            self.sum = self.sum.wrapping_add(self.i).wrapping_add(bonus);
            self.i += 1;
            if self.i < self.n {
                CoroutineState::Yielded(self.sum)
            } else {
                self.status = CoroutineStatus::Done;
                CoroutineState::Complete(self.sum)
            }
        }

        fn status(&self) -> CoroutineStatus {
            self.status
        }
    }
}

/// Drives a unit-resume [`Coroutine`] to completion, summing the
/// yielded values.
pub fn drive<C>(mut c: C) -> u64
where
    C: Coroutine<(), Yield = u64, Return = ()>,
{
    let mut acc = 0u64;
    let mut st = c.start();
    loop {
        match st {
            CoroutineState::Yielded(v) => {
                acc = acc.wrapping_add(v);
                st = c.resume(());
            }
            CoroutineState::Complete(()) => return acc,
        }
    }
}

/// Drives a resume-value [`Coroutine`] to completion, feeding a
/// function of each yielded value back in and summing everything.
pub fn drive_total<C>(mut c: C) -> u64
where
    C: Coroutine<u64, Yield = u64, Return = u64>,
{
    let mut acc = 0u64;
    let mut st = c.start();
    loop {
        match st {
            CoroutineState::Yielded(v) => {
                acc = acc.wrapping_add(v);
                st = c.resume(v & 0x7);
            }
            CoroutineState::Complete(r) => return acc.wrapping_add(r),
        }
    }
}

/// [`drive`] for genawaiter's resume protocol.
pub fn drive_ga<F>(mut g: genawaiter::rc::Gen<u64, (), F>) -> u64
where
    F: std::future::Future<Output = ()>,
{
    let mut acc = 0u64;
    loop {
        match g.resume_with(()) {
            genawaiter::GeneratorState::Yielded(v) => acc = acc.wrapping_add(v),
            genawaiter::GeneratorState::Complete(()) => return acc,
        }
    }
}

/// [`drive_total`] for genawaiter's resume protocol.
pub fn drive_ga_total<F>(mut g: genawaiter::rc::Gen<u64, u64, F>) -> u64
where
    F: std::future::Future<Output = u64>,
{
    let mut acc = 0u64;
    // genawaiter has no separate `start`; the first resume value is
    // discarded because no `yield_` is awaiting it yet.
    let mut st = g.resume_with(0);
    loop {
        match st {
            genawaiter::GeneratorState::Yielded(v) => {
                acc = acc.wrapping_add(v);
                st = g.resume_with(v & 0x7);
            }
            genawaiter::GeneratorState::Complete(r) => return acc.wrapping_add(r),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All three implementations of each workload produce the same
    /// driver result for a range of sizes.
    #[test]
    fn implementations_agree() {
        for n in [0, 1, 2, 7, 100] {
            let expected = drive(hand::counter(n));
            assert_eq!(drive(dia::counter(n)), expected, "counter({n})");
            assert_eq!(drive_ga(ga::counter(n)), expected, "counter({n})");

            let expected = drive(hand::nested(n));
            assert_eq!(drive(dia::nested(n)), expected, "nested({n})");
            assert_eq!(drive_ga(ga::nested(n)), expected, "nested({n})");

            let expected = drive_total(hand::running_total(n));
            assert_eq!(
                drive_total(dia::running_total(n)),
                expected,
                "running_total({n})"
            );
            assert_eq!(
                drive_ga_total(ga::running_total(n)),
                expected,
                "running_total({n})"
            );
        }
    }
}
