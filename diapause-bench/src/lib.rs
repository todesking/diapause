//! Benchmark workloads comparing `#[diapause::coroutine]`-generated
//! state machines against other generator/coroutine crates and
//! handwritten state machines.
//!
//! Each workload exists in several implementations with identical
//! observable behavior (verified by the tests at the bottom):
//!
//! - [`dia`](self::dia): `#[diapause::coroutine]`-generated state
//!   machines
//! - [`hand`](self::hand): handwritten state machines implementing
//!   [`diapause::Coroutine`]
//! - [`ga`](self::ga): genawaiter (`rc::Gen`; the `stack::Gen` variants
//!   are built inline in the benches because they cannot be returned by
//!   value)
//! - [`cs`](self::cs): corosensei (stackful; each construction
//!   allocates a fresh stack)
//! - [`gtor`](self::gtor): the `generator` crate (stackful; each
//!   construction allocates a fresh stack)
//! - [`ng`](self::ng): next-gen (proc-macro over an async transform;
//!   stack-pinned, so construction and driving are fused into one
//!   function per workload)
//!
//! Workloads:
//!
//! - `counter(n)`: yields `0..n` — the minimal resume/yield round trip.
//! - `nested(n)`: two nested loops, yields `i ^ j` for `j < i < n` —
//!   suspension points under nested control flow.
//! - `running_total(n)`: yields a running sum and folds the resume
//!   argument back in — exercises resume values and a completion value.
//! - `large_state(n)`: a `[u64; 32]` buffer stays live across every
//!   yield — exercises the cost of suspending with a large state.

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

    #[diapause::coroutine(yield = u64)]
    pub fn large_state(n: u64) {
        let mut buf: [u64; 32] = [0; 32];
        for i in 0..n {
            let idx = (i & 31) as usize;
            buf[idx] = buf[idx].wrapping_add(i);
            yield_!(buf[idx]);
        }
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

    pub fn large_state(n: u64) -> Gen<u64, (), impl Future<Output = ()>> {
        Gen::new(move |co| async move {
            let mut buf = [0u64; 32];
            for i in 0..n {
                let idx = (i & 31) as usize;
                buf[idx] = buf[idx].wrapping_add(i);
                co.yield_(buf[idx]).await;
            }
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

    pub struct LargeState {
        n: u64,
        i: u64,
        buf: [u64; 32],
        status: CoroutineStatus,
    }

    pub fn large_state(n: u64) -> LargeState {
        LargeState {
            n,
            i: 0,
            buf: [0; 32],
            status: CoroutineStatus::NotStarted,
        }
    }

    impl LargeState {
        fn step(&mut self) -> CoroutineState<u64, ()> {
            if self.i < self.n {
                let idx = (self.i & 31) as usize;
                self.buf[idx] = self.buf[idx].wrapping_add(self.i);
                self.i += 1;
                self.status = CoroutineStatus::Suspended;
                CoroutineState::Yielded(self.buf[idx])
            } else {
                self.status = CoroutineStatus::Done;
                CoroutineState::Complete(())
            }
        }
    }

    impl Coroutine for LargeState {
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
}

/// corosensei implementations (stackful; `Coroutine::new` allocates a
/// fresh stack, which the drive protocol includes in the measurement
/// like every other construction cost).
pub mod cs {
    use corosensei::Coroutine;

    pub fn counter(n: u64) -> Coroutine<(), u64, ()> {
        Coroutine::new(move |yielder, _: ()| {
            for i in 0..n {
                yielder.suspend(i);
            }
        })
    }

    pub fn nested(n: u64) -> Coroutine<(), u64, ()> {
        Coroutine::new(move |yielder, _: ()| {
            for i in 0..n {
                for j in 0..i {
                    yielder.suspend(i ^ j);
                }
            }
        })
    }

    pub fn running_total(n: u64) -> Coroutine<u64, u64, u64> {
        Coroutine::new(move |yielder, _first: u64| {
            let mut sum: u64 = 0;
            for i in 0..n {
                let bonus = yielder.suspend(sum);
                sum = sum.wrapping_add(i).wrapping_add(bonus);
            }
            sum
        })
    }

    pub fn large_state(n: u64) -> Coroutine<(), u64, ()> {
        Coroutine::new(move |yielder, _: ()| {
            let mut buf = [0u64; 32];
            for i in 0..n {
                let idx = (i & 31) as usize;
                buf[idx] = buf[idx].wrapping_add(i);
                yielder.suspend(buf[idx]);
            }
        })
    }
}

/// `generator` crate implementations (stackful; construction allocates
/// a fresh stack). The crate has no separate completion channel for
/// unit returns, so the unit workloads terminate with `done!()` and the
/// resume-value workload delivers its final sum as the last `send`
/// result.
pub mod gtor {
    use generator::{Generator, Gn, done};

    pub fn counter(n: u64) -> Generator<'static, (), u64> {
        Gn::new_scoped(move |mut s| {
            for i in 0..n {
                s.yield_(i);
            }
            done!()
        })
    }

    pub fn nested(n: u64) -> Generator<'static, (), u64> {
        Gn::new_scoped(move |mut s| {
            for i in 0..n {
                for j in 0..i {
                    s.yield_(i ^ j);
                }
            }
            done!()
        })
    }

    pub fn running_total(n: u64) -> Generator<'static, u64, u64> {
        Gn::new_scoped(move |mut s| {
            let mut sum: u64 = 0;
            for i in 0..n {
                let bonus = s.yield_(sum).expect("driver always sends");
                sum = sum.wrapping_add(i).wrapping_add(bonus);
            }
            sum
        })
    }

    pub fn large_state(n: u64) -> Generator<'static, (), u64> {
        Gn::new_scoped(move |mut s| {
            let mut buf = [0u64; 32];
            for i in 0..n {
                let idx = (i & 31) as usize;
                buf[idx] = buf[idx].wrapping_add(i);
                s.yield_(buf[idx]);
            }
            done!()
        })
    }
}

/// next-gen implementations. next-gen generators are stack-pinned
/// (`mk_gen!`), so they cannot be returned by value; construction and
/// the drive loop are fused into one `*_sum` function per workload,
/// mirroring what the benches measure for every other implementation.
pub mod ng {
    use next_gen::prelude::*;

    #[generator(yield(u64))]
    fn counter_gen(n: u64) {
        for i in 0..n {
            yield_!(i);
        }
    }

    pub fn counter_sum(n: u64) -> u64 {
        mk_gen!(let mut g = counter_gen(n));
        let mut acc = 0u64;
        loop {
            match g.as_mut().resume(()) {
                GeneratorState::Yielded(v) => acc = acc.wrapping_add(v),
                GeneratorState::Returned(()) => return acc,
            }
        }
    }

    #[generator(yield(u64))]
    fn nested_gen(n: u64) {
        for i in 0..n {
            for j in 0..i {
                yield_!(i ^ j);
            }
        }
    }

    pub fn nested_sum(n: u64) -> u64 {
        mk_gen!(let mut g = nested_gen(n));
        let mut acc = 0u64;
        loop {
            match g.as_mut().resume(()) {
                GeneratorState::Yielded(v) => acc = acc.wrapping_add(v),
                GeneratorState::Returned(()) => return acc,
            }
        }
    }

    #[generator(yield(u64), resume(u64) as _first)]
    fn running_total_gen(n: u64) -> u64 {
        let mut sum: u64 = 0;
        for i in 0..n {
            let bonus = yield_!(sum);
            sum = sum.wrapping_add(i).wrapping_add(bonus);
        }
        sum
    }

    pub fn running_total_sum(n: u64) -> u64 {
        mk_gen!(let mut g = running_total_gen(n));
        let mut acc = 0u64;
        // Like genawaiter, the first resume value is the (discarded)
        // initial argument.
        let mut st = g.as_mut().resume(0);
        loop {
            match st {
                GeneratorState::Yielded(v) => {
                    acc = acc.wrapping_add(v);
                    st = g.as_mut().resume(v & 0x7);
                }
                GeneratorState::Returned(r) => return acc.wrapping_add(r),
            }
        }
    }

    #[generator(yield(u64))]
    fn large_state_gen(n: u64) {
        let mut buf = [0u64; 32];
        for i in 0..n {
            let idx = (i & 31) as usize;
            buf[idx] = buf[idx].wrapping_add(i);
            yield_!(buf[idx]);
        }
    }

    pub fn large_state_sum(n: u64) -> u64 {
        mk_gen!(let mut g = large_state_gen(n));
        let mut acc = 0u64;
        loop {
            match g.as_mut().resume(()) {
                GeneratorState::Yielded(v) => acc = acc.wrapping_add(v),
                GeneratorState::Returned(()) => return acc,
            }
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

/// [`drive`] for corosensei's resume protocol.
pub fn drive_cs(mut c: corosensei::Coroutine<(), u64, ()>) -> u64 {
    let mut acc = 0u64;
    loop {
        match c.resume(()) {
            corosensei::CoroutineResult::Yield(v) => acc = acc.wrapping_add(v),
            corosensei::CoroutineResult::Return(()) => return acc,
        }
    }
}

/// [`drive_total`] for corosensei's resume protocol. The first resume
/// value becomes the closure's (discarded) initial argument, as with
/// genawaiter.
pub fn drive_cs_total(mut c: corosensei::Coroutine<u64, u64, u64>) -> u64 {
    let mut acc = 0u64;
    let mut st = c.resume(0);
    loop {
        match st {
            corosensei::CoroutineResult::Yield(v) => {
                acc = acc.wrapping_add(v);
                st = c.resume(v & 0x7);
            }
            corosensei::CoroutineResult::Return(r) => return acc.wrapping_add(r),
        }
    }
}

/// [`drive`] for the `generator` crate's resume protocol.
pub fn drive_gtor(mut g: generator::Generator<'static, (), u64>) -> u64 {
    let mut acc = 0u64;
    while let Some(v) = g.resume() {
        acc = acc.wrapping_add(v);
    }
    acc
}

/// [`drive_total`] for the `generator` crate's resume protocol: the
/// first `send` starts the generator (its value is discarded), and the
/// closure's return value arrives as the result of the final `send`.
pub fn drive_gtor_total(mut g: generator::Generator<'static, u64, u64>) -> u64 {
    let mut acc = 0u64;
    let mut v = g.send(0);
    loop {
        if g.is_done() {
            return acc.wrapping_add(v);
        }
        acc = acc.wrapping_add(v);
        v = g.send(v & 0x7);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All implementations of each workload produce the same driver
    /// result for a range of sizes.
    #[test]
    fn implementations_agree() {
        for n in [0, 1, 2, 7, 100] {
            let expected = drive(hand::counter(n));
            assert_eq!(drive(dia::counter(n)), expected, "counter({n})");
            assert_eq!(drive_ga(ga::counter(n)), expected, "counter({n})");
            assert_eq!(drive_cs(cs::counter(n)), expected, "counter({n})");
            assert_eq!(drive_gtor(gtor::counter(n)), expected, "counter({n})");
            assert_eq!(ng::counter_sum(n), expected, "counter({n})");

            let expected = drive(hand::nested(n));
            assert_eq!(drive(dia::nested(n)), expected, "nested({n})");
            assert_eq!(drive_ga(ga::nested(n)), expected, "nested({n})");
            assert_eq!(drive_cs(cs::nested(n)), expected, "nested({n})");
            assert_eq!(drive_gtor(gtor::nested(n)), expected, "nested({n})");
            assert_eq!(ng::nested_sum(n), expected, "nested({n})");

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
            assert_eq!(
                drive_cs_total(cs::running_total(n)),
                expected,
                "running_total({n})"
            );
            assert_eq!(
                drive_gtor_total(gtor::running_total(n)),
                expected,
                "running_total({n})"
            );
            assert_eq!(ng::running_total_sum(n), expected, "running_total({n})");

            let expected = drive(hand::large_state(n));
            assert_eq!(drive(dia::large_state(n)), expected, "large_state({n})");
            assert_eq!(drive_ga(ga::large_state(n)), expected, "large_state({n})");
            assert_eq!(drive_cs(cs::large_state(n)), expected, "large_state({n})");
            assert_eq!(
                drive_gtor(gtor::large_state(n)),
                expected,
                "large_state({n})"
            );
            assert_eq!(ng::large_state_sum(n), expected, "large_state({n})");
        }
    }
}
