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

/// Iteration with for loop.
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

/// Iter deref access to underlying coroutine.
#[test]
fn iter_deref_access() {
    #[diapause::coroutine(yield = u32, resume = ())]
    fn single_yield() {
        yield_!(42);
    }

    let mut iter = diapause::Iter::new(single_yield());
    // Deref to access coroutine's methods (if needed)
    // Just test that Deref works
    let _c: &_ = &*iter;
    assert_eq!(iter.next(), Some(42));
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
