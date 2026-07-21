use diapause::yield_;

fn not_a_coroutine() {
    yield_!(1);
}

fn main() {}
