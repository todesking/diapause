/// Empty coroutine yields nothing and completes immediately.
#[test]
fn iter_empty_coroutine() {
    #[diapause::coroutine(yield = u32, resume = ())]
    fn empty() {}

    let mut iter = diapause::Iter::new(empty());
    assert_eq!(iter.next(), None);
}

/// Basic iteration over yielded values.
#[test]
fn iter_basic_iteration() {
    #[diapause::coroutine(yield = u32, resume = ())]
    fn count() {
        let nums: [u32; 3] = [1, 2, 3];
        for n in nums {
            yield_!(n);
        }
    }

    let mut iter = diapause::Iter::new(count());
    assert_eq!(iter.next(), Some(1));
    assert_eq!(iter.next(), Some(2));
    assert_eq!(iter.next(), Some(3));
    assert_eq!(iter.next(), None);
    // Fused behavior: subsequent calls also return None
    assert_eq!(iter.next(), None);
}

/// Iteration with for loop over an explicit `Iter::new`.
#[test]
fn iter_for_loop() {
    #[diapause::coroutine(yield = u32, resume = ())]
    fn count_to_five() {
        let nums: [u32; 5] = [1, 2, 3, 4, 5];
        for n in nums {
            yield_!(n);
        }
    }

    let mut sum = 0;
    for n in diapause::Iter::new(count_to_five()) {
        sum += n;
    }
    assert_eq!(sum, 15);
}

/// A `resume = ()` coroutine implements `IntoIterator`, so it can be
/// passed directly to a `for` loop without `Iter::new`.
#[test]
fn into_iterator_direct_for_loop() {
    #[diapause::coroutine(yield = u32, resume = ())]
    fn count_to_five() {
        let nums: [u32; 5] = [1, 2, 3, 4, 5];
        for n in nums {
            yield_!(n);
        }
    }

    let mut sum = 0;
    for n in count_to_five() {
        sum += n;
    }
    assert_eq!(sum, 15);
}

/// `IntoIterator::into_iter` yields a `diapause::Iter` that can be driven
/// with `next`.
#[test]
fn into_iterator_into_iter_call() {
    #[diapause::coroutine(yield = u32, resume = ())]
    fn count() {
        let nums: [u32; 2] = [7, 8];
        for n in nums {
            yield_!(n);
        }
    }

    let mut iter = count().into_iter();
    assert_eq!(iter.next(), Some(7));
    assert_eq!(iter.next(), Some(8));
    assert_eq!(iter.next(), None);
}

/// Omitting `resume` (defaulting to `()`) still generates `IntoIterator`.
#[test]
fn into_iterator_default_resume() {
    #[diapause::coroutine(yield = u32)]
    fn count() {
        let nums: [u32; 2] = [1, 2];
        for n in nums {
            yield_!(n);
        }
    }

    let collected: u32 = count().into_iter().sum();
    assert_eq!(collected, 3);
}

/// Iter can be used as an IntoIterator (since Iterator auto-implements it).
#[test]
fn iter_into_iterator_auto_impl() {
    #[diapause::coroutine(yield = i32, resume = ())]
    fn negatives() {
        let nums: [i32; 2] = [-1, -2];
        for n in nums {
            yield_!(n);
        }
    }

    let iter = diapause::Iter::new(negatives());
    let mut collected = [0; 2];
    let mut idx = 0;
    for n in iter {
        if idx < collected.len() {
            collected[idx] = n;
            idx += 1;
        }
    }
    assert_eq!(collected, [-1, -2]);
}

/// Multiple iterations over the same Iter (consumed).
#[test]
fn iter_consumed_after_iteration() {
    #[diapause::coroutine(yield = u32, resume = ())]
    fn count() {
        let nums: [u32; 2] = [1, 2];
        for n in nums {
            yield_!(n);
        }
    }

    let mut iter = diapause::Iter::new(count());
    // First iteration
    assert_eq!(iter.next(), Some(1));
    assert_eq!(iter.next(), Some(2));
    assert_eq!(iter.next(), None);

    // Second iteration (should remain exhausted)
    assert_eq!(iter.next(), None);
}

/// `get_ref` exposes the wrapped coroutine for inspection (e.g. its
/// status) without consuming the iterator.
#[test]
fn iter_get_ref() {
    use diapause::{Coroutine, CoroutineStatus};

    #[diapause::coroutine(yield = u32, resume = ())]
    fn single_yield() {
        yield_!(42);
    }

    let mut iter = diapause::Iter::new(single_yield());
    assert_eq!(iter.get_ref().status(), CoroutineStatus::NotStarted);
    assert_eq!(iter.next(), Some(42));
    assert_eq!(iter.get_ref().status(), CoroutineStatus::Suspended);
    assert_eq!(iter.next(), None);
    assert_eq!(iter.get_ref().status(), CoroutineStatus::Done);
}

/// Driving the coroutine directly through `get_mut` stays consistent with
/// continued iteration, because `next` re-derives its action from the
/// coroutine's `status()` rather than any shadow state in the `Iter`.
#[test]
fn iter_get_mut_stays_consistent() {
    use diapause::{Coroutine, CoroutineState};

    #[diapause::coroutine(yield = u32, resume = ())]
    fn count() {
        let nums: [u32; 4] = [1, 2, 3, 4];
        for n in nums {
            yield_!(n);
        }
    }

    let mut iter = diapause::Iter::new(count());
    // Drive the first step directly through the coroutine.
    assert_eq!(iter.get_mut().start(), CoroutineState::Yielded(1));
    // The Iter picks up seamlessly from the coroutine's real status.
    assert_eq!(iter.next(), Some(2));
    // Drive another step directly.
    assert_eq!(iter.get_mut().resume(()), CoroutineState::Yielded(3));
    assert_eq!(iter.next(), Some(4));
    assert_eq!(iter.next(), None);
}

/// A `Poisoned` coroutine makes `next` panic rather than silently ending.
#[test]
#[should_panic(expected = "Poisoned")]
fn iter_poisoned_panics() {
    use diapause::Coroutine;

    #[diapause::coroutine(yield = u32, resume = ())]
    fn boom() {
        let vals: [u32; 1] = [1];
        let divisor = 0u32;
        for v in vals {
            // Panics while evaluating the yield expression during `start`.
            yield_!(v / divisor);
        }
    }

    let mut iter = diapause::Iter::new(boom());
    // First `next` runs `start`, which panics inside the transition and
    // leaves the coroutine Poisoned.
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| iter.next()));
    assert!(poisoned.is_err());
    assert_eq!(iter.get_ref().status(), diapause::CoroutineStatus::Poisoned);
    // The next call observes Poisoned and re-panics.
    let _ = iter.next();
}

/// Iter can be converted back to inner coroutine.
#[test]
fn iter_into_inner() {
    #[diapause::coroutine(yield = u32, resume = ())]
    fn count() {
        let nums: [u32; 1] = [100];
        for n in nums {
            yield_!(n);
        }
    }

    let iter = diapause::Iter::new(count());
    let _c = iter.into_inner();
    // Can't use c afterward since it's consumed, but we've proven the method exists
}

/// `IntoIterator` is generated for generic coroutines too, with the
/// generics propagated onto the impl.
#[test]
fn into_iterator_generic_coroutine() {
    #[diapause::coroutine(yield = T, resume = ())]
    fn repeat<T: Clone>(value: T) {
        yield_!(value.clone());
        yield_!(value);
    }

    let collected: Vec<u32> = repeat(9u32).into_iter().collect();
    assert_eq!(collected, [9, 9]);

    let words: Vec<String> = repeat("hi".to_string()).into_iter().collect();
    assert_eq!(words, ["hi", "hi"]);
}

/// `Iter::new(&mut c)` borrows the coroutine (via the `Coroutine for
/// `&mut C` forwarding impl), so a `for` loop can iterate partially
/// without consuming it.
#[test]
fn iter_by_mut_ref_partial_iteration() {
    use diapause::{Coroutine, CoroutineState, CoroutineStatus};

    #[diapause::coroutine(yield = u32, resume = ())]
    fn count() {
        let nums: [u32; 4] = [1, 2, 3, 4];
        for n in nums {
            yield_!(n);
        }
    }

    let mut c = count();
    let mut sum = 0;
    for n in diapause::Iter::new(&mut c) {
        sum += n;
        if n == 2 {
            break;
        }
    }
    assert_eq!(sum, 3);
    // The coroutine survives the loop, suspended, and can be driven on —
    // directly or through another borrowing `Iter`.
    assert_eq!(c.status(), CoroutineStatus::Suspended);
    assert_eq!(c.resume(()), CoroutineState::Yielded(3));
    let rest: Vec<u32> = diapause::Iter::new(&mut c).collect();
    assert_eq!(rest, [4]);
    assert_eq!(c.status(), CoroutineStatus::Done);
}

#[diapause::coroutine(yield = u32, resume = ())]
fn inner_yields() {
    let nums: [u32; 2] = [10, 20];
    for n in nums {
        yield_!(n);
    }
}

#[diapause::coroutine(yield = u32, resume = ())]
fn outer_with_delegation() {
    let g: inner_yields::State = inner_yields();
    yield_all!(g);
    yield_!(30);
}

/// Iteration with yield_all! delegation from helper coroutines.
#[test]
fn iter_with_yield_all_delegation() {
    let mut iter = diapause::Iter::new(outer_with_delegation());
    assert_eq!(iter.next(), Some(10));
    assert_eq!(iter.next(), Some(20));
    assert_eq!(iter.next(), Some(30));
    assert_eq!(iter.next(), None);
}
