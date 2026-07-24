//! Resume/yield throughput of diapause coroutines vs genawaiter
//! (`rc::Gen` and `stack::Gen`), corosensei, the `generator` crate,
//! next-gen, and handwritten state machines.
//!
//! Every bench constructs the coroutine and drives it to completion,
//! summing the yielded values; throughput is reported per yielded
//! element. For the stackful crates (corosensei, generator) the
//! construction inside the loop includes allocating a fresh stack.
//! See `docs/benchmarks.md` for recorded results.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use diapause_bench::{
    cs, dia, drive, drive_cs, drive_cs_total, drive_ga, drive_ga_total, drive_gtor,
    drive_gtor_total, drive_total, ga, gtor, hand, ng,
};
use genawaiter::GeneratorState;

/// Yields per iteration for `counter`, `running_total`, and
/// `large_state`.
const N: u64 = 1024;
/// Outer loop bound for `nested`: yields n*(n-1)/2 = 2016 elements.
const NESTED_N: u64 = 64;

fn bench_counter(c: &mut Criterion) {
    let mut g = c.benchmark_group("counter");
    g.throughput(Throughput::Elements(N));
    g.bench_function("diapause", |b| b.iter(|| drive(dia::counter(black_box(N)))));
    g.bench_function("handwritten", |b| {
        b.iter(|| drive(hand::counter(black_box(N))))
    });
    g.bench_function("genawaiter_rc", |b| {
        b.iter(|| drive_ga(ga::counter(black_box(N))))
    });
    g.bench_function("genawaiter_stack", |b| {
        b.iter(|| {
            let n = black_box(N);
            genawaiter::stack::let_gen_using!(sgen, |co| async move {
                for i in 0..n {
                    co.yield_(i).await;
                }
            });
            let mut acc = 0u64;
            loop {
                match sgen.resume_with(()) {
                    GeneratorState::Yielded(v) => acc = acc.wrapping_add(v),
                    GeneratorState::Complete(()) => return acc,
                }
            }
        })
    });
    g.bench_function("corosensei", |b| {
        b.iter(|| drive_cs(cs::counter(black_box(N))))
    });
    g.bench_function("generator", |b| {
        b.iter(|| drive_gtor(gtor::counter(black_box(N))))
    });
    g.bench_function("next_gen", |b| b.iter(|| ng::counter_sum(black_box(N))));
    g.finish();
}

fn bench_nested(c: &mut Criterion) {
    let mut g = c.benchmark_group("nested");
    g.throughput(Throughput::Elements(NESTED_N * (NESTED_N - 1) / 2));
    g.bench_function("diapause", |b| {
        b.iter(|| drive(dia::nested(black_box(NESTED_N))))
    });
    g.bench_function("handwritten", |b| {
        b.iter(|| drive(hand::nested(black_box(NESTED_N))))
    });
    g.bench_function("genawaiter_rc", |b| {
        b.iter(|| drive_ga(ga::nested(black_box(NESTED_N))))
    });
    g.bench_function("genawaiter_stack", |b| {
        b.iter(|| {
            let n = black_box(NESTED_N);
            genawaiter::stack::let_gen_using!(sgen, |co| async move {
                for i in 0..n {
                    for j in 0..i {
                        co.yield_(i ^ j).await;
                    }
                }
            });
            let mut acc = 0u64;
            loop {
                match sgen.resume_with(()) {
                    GeneratorState::Yielded(v) => acc = acc.wrapping_add(v),
                    GeneratorState::Complete(()) => return acc,
                }
            }
        })
    });
    g.bench_function("corosensei", |b| {
        b.iter(|| drive_cs(cs::nested(black_box(NESTED_N))))
    });
    g.bench_function("generator", |b| {
        b.iter(|| drive_gtor(gtor::nested(black_box(NESTED_N))))
    });
    g.bench_function("next_gen", |b| {
        b.iter(|| ng::nested_sum(black_box(NESTED_N)))
    });
    g.finish();
}

fn bench_running_total(c: &mut Criterion) {
    let mut g = c.benchmark_group("running_total");
    g.throughput(Throughput::Elements(N));
    g.bench_function("diapause", |b| {
        b.iter(|| drive_total(dia::running_total(black_box(N))))
    });
    g.bench_function("handwritten", |b| {
        b.iter(|| drive_total(hand::running_total(black_box(N))))
    });
    g.bench_function("genawaiter_rc", |b| {
        b.iter(|| drive_ga_total(ga::running_total(black_box(N))))
    });
    g.bench_function("genawaiter_stack", |b| {
        b.iter(|| {
            let n = black_box(N);
            genawaiter::stack::let_gen_using!(sgen, |co| async move {
                let mut sum: u64 = 0;
                for i in 0..n {
                    let bonus = co.yield_(sum).await;
                    sum = sum.wrapping_add(i).wrapping_add(bonus);
                }
                sum
            });
            let mut acc = 0u64;
            let mut st = sgen.resume_with(0);
            loop {
                match st {
                    GeneratorState::Yielded(v) => {
                        acc = acc.wrapping_add(v);
                        st = sgen.resume_with(v & 0x7);
                    }
                    GeneratorState::Complete(r) => return acc.wrapping_add(r),
                }
            }
        })
    });
    g.bench_function("corosensei", |b| {
        b.iter(|| drive_cs_total(cs::running_total(black_box(N))))
    });
    g.bench_function("generator", |b| {
        b.iter(|| drive_gtor_total(gtor::running_total(black_box(N))))
    });
    g.bench_function("next_gen", |b| {
        b.iter(|| ng::running_total_sum(black_box(N)))
    });
    g.finish();
}

fn bench_large_state(c: &mut Criterion) {
    let mut g = c.benchmark_group("large_state");
    g.throughput(Throughput::Elements(N));
    g.bench_function("diapause", |b| {
        b.iter(|| drive(dia::large_state(black_box(N))))
    });
    g.bench_function("handwritten", |b| {
        b.iter(|| drive(hand::large_state(black_box(N))))
    });
    g.bench_function("genawaiter_rc", |b| {
        b.iter(|| drive_ga(ga::large_state(black_box(N))))
    });
    g.bench_function("genawaiter_stack", |b| {
        b.iter(|| {
            let n = black_box(N);
            genawaiter::stack::let_gen_using!(sgen, |co| async move {
                let mut buf = [0u64; 32];
                for i in 0..n {
                    let idx = (i & 31) as usize;
                    buf[idx] = buf[idx].wrapping_add(i);
                    co.yield_(buf[idx]).await;
                }
            });
            let mut acc = 0u64;
            loop {
                match sgen.resume_with(()) {
                    GeneratorState::Yielded(v) => acc = acc.wrapping_add(v),
                    GeneratorState::Complete(()) => return acc,
                }
            }
        })
    });
    g.bench_function("corosensei", |b| {
        b.iter(|| drive_cs(cs::large_state(black_box(N))))
    });
    g.bench_function("generator", |b| {
        b.iter(|| drive_gtor(gtor::large_state(black_box(N))))
    });
    g.bench_function("next_gen", |b| b.iter(|| ng::large_state_sum(black_box(N))));
    g.finish();
}

criterion_group!(
    benches,
    bench_counter,
    bench_nested,
    bench_running_total,
    bench_large_state
);
criterion_main!(benches);
