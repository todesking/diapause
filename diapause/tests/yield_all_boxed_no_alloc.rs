//! Pins the lazy-boxing guarantee of `yield_all!(box sub)`: the box is
//! allocated only when the delegated coroutine actually suspends, so a
//! delegate that completes on entry allocates nothing.
//!
//! Kept in its own integration-test binary so the counting global
//! allocator sees no unrelated test running concurrently.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingAlloc;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static A: CountingAlloc = CountingAlloc;

use diapause::{Coroutine, CoroutineState};

#[diapause::coroutine(yield = u32)]
fn completes_immediately(n: u32) -> u32 {
    n
}

#[diapause::coroutine(yield = u32)]
fn suspends_once(n: u32) -> u32 {
    yield_!(n);
    n
}

#[diapause::coroutine(yield = u32)]
fn outer_immediate(n: u32) -> u32 {
    let sub: completes_immediately::State = completes_immediately(n);
    yield_all!(box sub)
}

#[diapause::coroutine(yield = u32)]
fn outer_suspending(n: u32) -> u32 {
    let sub: suspends_once::State = suspends_once(n);
    yield_all!(box sub)
}

fn alloc_count(f: impl FnOnce()) -> usize {
    let before = ALLOCS.load(Ordering::SeqCst);
    f();
    ALLOCS.load(Ordering::SeqCst) - before
}

#[test]
fn boxing_is_lazy() {
    // Completing on entry stays on the unboxed path: no allocation.
    let immediate = alloc_count(|| {
        let mut c = outer_immediate(7);
        assert_eq!(c.start(), CoroutineState::Complete(7));
    });
    assert_eq!(immediate, 0);

    // Suspending crosses into the boxed state: exactly one allocation.
    let suspending = alloc_count(|| {
        let mut c = outer_suspending(7);
        assert_eq!(c.start(), CoroutineState::Yielded(7));
        assert_eq!(c.resume(()), CoroutineState::Complete(7));
    });
    assert_eq!(suspending, 1);
}
