#![no_std]

#[diapause::coroutine(yield = u32)]
fn simple_counter() -> u32 {
    yield_!(1);
    yield_!(2);
    3
}

#[cfg(test)]
mod tests {
    use super::*;
    use diapause::{Coroutine, CoroutineState};

    #[test]
    fn coroutine_works_in_no_std() {
        let mut c = simple_counter();
        assert_eq!(c.start(), CoroutineState::Yielded(1));
        assert_eq!(c.resume(()), CoroutineState::Yielded(2));
        assert_eq!(c.resume(()), CoroutineState::Complete(3));
    }
}
