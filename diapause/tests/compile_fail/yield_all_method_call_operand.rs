struct Factory;

impl Factory {
    fn make(&self) -> u32 {
        0
    }
}

#[diapause::coroutine(yield = u32)]
fn outer(f: Factory) -> u32 {
    let v: u32 = yield_all!(f.make());
    v
}

fn main() {}
