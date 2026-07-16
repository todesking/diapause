fn helper(n: u32) -> Option<u32> {
    Some(n)
}

#[baregen::coroutine(yield = u32)]
fn shadowed(n: u32) -> u32 {
    let mut sum: u32 = 0;
    loop {
        yield_!(sum);
        if let Some(sum) = helper(n) {
            if sum > 3 {
                break;
            }
        }
        sum += 1;
    }
    sum
}

fn main() {}
