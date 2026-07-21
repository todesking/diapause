//! The `?` operator inside coroutine bodies: Result and Option
//! coroutines, error conversion, and interaction with yields.

use diapause::{Coroutine, CoroutineState};

#[derive(Debug, PartialEq)]
pub struct ParseError;

fn parse(s: &str) -> Result<u32, ParseError> {
    s.parse().map_err(|_| ParseError)
}

#[diapause::coroutine(yield = u32)]
fn sum_two(a: &'static str, b: &'static str) -> Result<u32, ParseError> {
    let x: u32 = parse(a)?;
    yield_!(x);
    let y: u32 = parse(b)?;
    Ok(x + y)
}

#[test]
fn result_success_path() {
    let mut c = sum_two("1", "2");
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(Ok(3)));
}

#[test]
fn result_failure_before_first_yield() {
    let mut c = sum_two("x", "2");
    assert_eq!(c.start(), CoroutineState::Complete(Err(ParseError)));
}

#[test]
fn result_failure_after_resume() {
    let mut c = sum_two("1", "x");
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(Err(ParseError)));
}

/// A completing `?` transitions to Done; further resumes panic as after
/// a normal completion.
#[test]
#[should_panic(expected = "Already done")]
fn resume_after_try_exit_panics() {
    let mut c = sum_two("x", "2");
    let _ = c.start();
    let _ = c.resume(());
}

#[derive(Debug, PartialEq)]
pub struct WrappedError(&'static str);

impl From<ParseError> for WrappedError {
    fn from(_: ParseError) -> Self {
        WrappedError("bad number")
    }
}

/// `?` converts the error through `From`, as in a plain function.
#[diapause::coroutine(yield = u32)]
fn converts_error(s: &'static str) -> Result<u32, WrappedError> {
    let x: u32 = parse(s)?;
    yield_!(x);
    Ok(x)
}

#[test]
fn result_from_conversion() {
    let mut c = converts_error("nope");
    assert_eq!(
        c.start(),
        CoroutineState::Complete(Err(WrappedError("bad number")))
    );
}

#[diapause::coroutine(yield = u32, resume = u32)]
fn option_try(start: Option<u32>) -> Option<u32> {
    let x: u32 = start?;
    let r = yield_!(x);
    Some(x.checked_add(r)?)
}

#[test]
fn option_success_path() {
    let mut c = option_try(Some(4));
    assert_eq!(c.start(), CoroutineState::Yielded(4));
    assert_eq!(c.resume(1), CoroutineState::Complete(Some(5)));
}

#[test]
fn option_none_short_circuits() {
    let mut c = option_try(None);
    assert_eq!(c.start(), CoroutineState::Complete(None));
}

#[test]
fn option_none_in_yield_value() {
    let mut c = option_try(Some(u32::MAX));
    assert_eq!(c.start(), CoroutineState::Yielded(u32::MAX));
    assert_eq!(c.resume(1), CoroutineState::Complete(None));
}

/// `?` → yield → `?`, with the second `?` on a value computed from the
/// resume argument.
#[diapause::coroutine(yield = u32, resume = &'static str)]
fn try_yield_try(a: &'static str) -> Result<u32, ParseError> {
    let x: u32 = parse(a)?;
    let b = yield_!(x);
    let y: u32 = parse(b)?;
    Ok(x * 100 + y)
}

#[test]
fn try_then_yield_then_try() {
    let mut c = try_yield_try("7");
    assert_eq!(c.start(), CoroutineState::Yielded(7));
    assert_eq!(c.resume("42"), CoroutineState::Complete(Ok(742)));

    let mut c = try_yield_try("7");
    assert_eq!(c.start(), CoroutineState::Yielded(7));
    assert_eq!(c.resume("x"), CoroutineState::Complete(Err(ParseError)));
}

/// `?` inside an if that contains no yield (an opaque statement).
#[diapause::coroutine(yield = u32)]
fn try_in_opaque_if(s: &'static str, check: bool) -> Result<u32, ParseError> {
    let mut x: u32 = 1;
    if check {
        x += parse(s)?;
    } else {
        x += 100;
    }
    yield_!(x);
    Ok(x)
}

#[test]
fn try_inside_opaque_if() {
    let mut c = try_in_opaque_if("9", true);
    assert_eq!(c.start(), CoroutineState::Yielded(10));
    assert_eq!(c.resume(()), CoroutineState::Complete(Ok(10)));

    let mut c = try_in_opaque_if("x", true);
    assert_eq!(c.start(), CoroutineState::Complete(Err(ParseError)));

    let mut c = try_in_opaque_if("x", false);
    assert_eq!(c.start(), CoroutineState::Yielded(101));
}

/// `?` inside the value expression of a yield.
#[diapause::coroutine(yield = u32)]
fn try_in_yield_value(s: &'static str) -> Result<u32, ParseError> {
    yield_!(parse(s)?);
    Ok(0)
}

#[test]
fn try_inside_yield_value() {
    let mut c = try_in_yield_value("3");
    assert_eq!(c.start(), CoroutineState::Yielded(3));
    assert_eq!(c.resume(()), CoroutineState::Complete(Ok(0)));

    let mut c = try_in_yield_value("x");
    assert_eq!(c.start(), CoroutineState::Complete(Err(ParseError)));
}

/// Nested `?`: the operand of a `?` itself contains a `?`.
#[diapause::coroutine(yield = u32)]
fn nested_try(a: Option<&'static str>) -> Result<u32, ParseError> {
    let x: u32 = parse(a.ok_or(ParseError)?)?;
    yield_!(x);
    Ok(x)
}

#[test]
fn nested_try_operands() {
    let mut c = nested_try(Some("5"));
    assert_eq!(c.start(), CoroutineState::Yielded(5));

    let mut c = nested_try(None);
    assert_eq!(c.start(), CoroutineState::Complete(Err(ParseError)));

    let mut c = nested_try(Some("x"));
    assert_eq!(c.start(), CoroutineState::Complete(Err(ParseError)));
}

/// `?` inside a closure belongs to the closure, not the coroutine.
#[diapause::coroutine(yield = u32)]
fn closure_try(s: &'static str) -> u32 {
    let double = |s: &str| -> Result<u32, ParseError> { Ok(parse(s)? * 2) };
    let x: u32 = double(s).unwrap_or(0);
    yield_!(x);
    x
}

#[test]
fn try_inside_closure_is_untouched() {
    let mut c = closure_try("21");
    assert_eq!(c.start(), CoroutineState::Yielded(42));
    assert_eq!(c.resume(()), CoroutineState::Complete(42));

    let mut c = closure_try("x");
    assert_eq!(c.start(), CoroutineState::Yielded(0));
}
