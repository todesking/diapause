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

// === `?` on a delegation's completion value ===

/// Errors before its first yield when `s` does not parse.
#[diapause::coroutine(yield = u32, resume = u32)]
fn try_sub(s: &'static str) -> Result<u32, ParseError> {
    let x: u32 = parse(s)?;
    let r = yield_!(x);
    Ok(x + r)
}

#[diapause::coroutine(yield = u32, resume = u32)]
fn delegate_try_let(s: &'static str) -> Result<u32, ParseError> {
    let sub: try_sub::State = try_sub(s);
    let v: u32 = yield_all!(sub)?;
    let r = yield_!(v);
    Ok(v + r)
}

#[test]
fn delegation_try_ok_path() {
    let mut c = delegate_try_let("7");
    assert_eq!(c.start(), CoroutineState::Yielded(7)); // sub yields x
    assert_eq!(c.resume(1), CoroutineState::Yielded(8)); // sub completes Ok(7+1), `?` unwraps
    assert_eq!(c.resume(2), CoroutineState::Complete(Ok(10)));
}

#[test]
fn delegation_try_err_short_circuits() {
    let mut c = delegate_try_let("x");
    assert_eq!(c.start(), CoroutineState::Complete(Err(ParseError)));
}

/// Yields once, then errors on resume: the delegation suspends before
/// the `?` takes the Err exit.
#[diapause::coroutine(yield = u32, resume = u32)]
fn yield_then_err(s: &'static str) -> Result<u32, ParseError> {
    let r = yield_!(0);
    let x: u32 = parse(s)?;
    Ok(x + r)
}

#[diapause::coroutine(yield = u32, resume = u32)]
fn delegate_try_mid(s: &'static str) -> Result<u32, ParseError> {
    let sub: yield_then_err::State = yield_then_err(s);
    let v: u32 = yield_all!(sub)?;
    Ok(v + 100)
}

#[test]
fn delegation_err_after_a_forwarded_yield() {
    let mut c = delegate_try_mid("x");
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(1), CoroutineState::Complete(Err(ParseError)));
}

/// A completing delegation `?` transitions to Done like any completion.
#[test]
#[should_panic(expected = "Already done")]
fn resume_after_delegation_try_exit_panics() {
    let mut c = delegate_try_mid("x");
    let _ = c.start();
    let _ = c.resume(1);
    let _ = c.resume(2);
}

/// Statement position: the Ok value (a non-`()` `u32`) is discarded,
/// the Err still exits early.
#[diapause::coroutine(yield = u32, resume = u32)]
fn delegate_try_stmt(s: &'static str) -> Result<u32, ParseError> {
    let sub: try_sub::State = try_sub(s);
    yield_all!(sub)?;
    Ok(1)
}

#[test]
fn statement_position_delegation_try() {
    let mut c = delegate_try_stmt("7");
    assert_eq!(c.start(), CoroutineState::Yielded(7));
    assert_eq!(c.resume(1), CoroutineState::Complete(Ok(1)));

    let mut c = delegate_try_stmt("x");
    assert_eq!(c.start(), CoroutineState::Complete(Err(ParseError)));
}

/// Tail position: the delegate completes with a nested
/// `Result<Result<u32, _>, _>`; the `?` unwraps the outer layer and the
/// inner Result is the coroutine's completion value.
#[diapause::coroutine(yield = u32, resume = u32)]
fn nested_result_sub(s: &'static str) -> Result<Result<u32, ParseError>, ParseError> {
    let r = yield_!(0);
    let x: u32 = parse(s)?;
    Ok(Ok(x + r))
}

#[diapause::coroutine(yield = u32, resume = u32)]
fn delegate_try_tail(s: &'static str) -> Result<u32, ParseError> {
    let sub: nested_result_sub::State = nested_result_sub(s);
    yield_all!(sub)?
}

#[test]
fn tail_position_delegation_try() {
    let mut c = delegate_try_tail("7");
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(1), CoroutineState::Complete(Ok(8)));

    let mut c = delegate_try_tail("x");
    assert_eq!(c.start(), CoroutineState::Yielded(0));
    assert_eq!(c.resume(1), CoroutineState::Complete(Err(ParseError)));
}

/// The `box` modifier combines with `?`.
#[diapause::coroutine(yield = u32, resume = u32)]
fn delegate_try_boxed(s: &'static str) -> Result<u32, ParseError> {
    let sub: try_sub::State = try_sub(s);
    let v: u32 = yield_all!(box sub)?;
    Ok(v)
}

#[test]
fn boxed_delegation_try() {
    let mut c = delegate_try_boxed("7");
    assert_eq!(c.start(), CoroutineState::Yielded(7));
    assert_eq!(c.resume(1), CoroutineState::Complete(Ok(8)));

    let mut c = delegate_try_boxed("x");
    assert_eq!(c.start(), CoroutineState::Complete(Err(ParseError)));
}

/// `yield_all_resume!` combines with `?` the same way.
#[diapause::coroutine(yield = u32, resume = u32)]
fn delegate_resume_try(sub: try_sub::State, first: u32) -> Result<u32, ParseError> {
    let v: u32 = yield_all_resume!(sub, first)?;
    Ok(v * 10)
}

#[test]
fn resume_delegation_try() {
    let mut sub = try_sub("7");
    assert_eq!(sub.start(), CoroutineState::Yielded(7));
    let mut c = delegate_resume_try(sub, 1); // sub completes Ok(7+1) on entry
    assert_eq!(c.start(), CoroutineState::Complete(Ok(80)));
}

// === `?` inside a `return` value ===

/// The manual-driver shape (a `loop { match sub.start()/resume() }`
/// forwarding yields) with `?` applied inside an unbraced match-arm
/// `return`. Regression: the expression-position return used to be
/// pre-rewritten into a completion block whose construction re-parsed
/// the `?`'s synthesized exit; the opaque rewriter then wrapped it in a
/// second completion and the generated code failed with E0308.
#[diapause::coroutine(yield = u32, resume = u32)]
fn drive(mut sub: try_sub::State) -> Result<u32, ParseError> {
    let mut step: CoroutineState<u32, Result<u32, ParseError>> = sub.start();
    loop {
        match step {
            CoroutineState::Complete(result) => return Ok(result? + 100),
            CoroutineState::Yielded(y) => {
                let r: u32 = yield_!(y);
                step = sub.resume(r);
            }
        }
    }
}

#[test]
fn try_in_a_match_arm_return() {
    let mut c = drive(try_sub("7"));
    assert_eq!(c.start(), CoroutineState::Yielded(7)); // sub yields x
    assert_eq!(c.resume(1), CoroutineState::Complete(Ok(108))); // Ok(7+1)? + 100

    let mut c = drive(try_sub("x"));
    assert_eq!(c.start(), CoroutineState::Complete(Err(ParseError)));
}

/// The braced form of the same arm goes through the statement-return
/// path (a `Return` terminator inside the arm's block).
#[diapause::coroutine(yield = u32, resume = u32)]
fn drive_braced(mut sub: try_sub::State) -> Result<u32, ParseError> {
    let mut step: CoroutineState<u32, Result<u32, ParseError>> = sub.start();
    loop {
        match step {
            CoroutineState::Complete(result) => {
                return Ok(result? + 100);
            }
            CoroutineState::Yielded(y) => {
                let r: u32 = yield_!(y);
                step = sub.resume(r);
            }
        }
    }
}

#[test]
fn try_in_a_braced_match_arm_return() {
    let mut c = drive_braced(try_sub("7"));
    assert_eq!(c.start(), CoroutineState::Yielded(7));
    assert_eq!(c.resume(1), CoroutineState::Complete(Ok(108)));

    let mut c = drive_braced(try_sub("x"));
    assert_eq!(c.start(), CoroutineState::Complete(Err(ParseError)));
}

/// The same arm return with `?` where the match is a `let` initializer:
/// the returning arm diverges, the yielding arm produces the value.
#[diapause::coroutine(yield = u32, resume = u32)]
fn try_in_stored_arm_return(r: Result<u32, ParseError>, flag: bool) -> Result<u32, ParseError> {
    let x: u32 = match flag {
        true => return Ok(r? + 1),
        false => {
            let v = yield_!(9);
            v
        }
    };
    Ok(x * 2)
}

#[test]
fn try_in_a_stored_arm_return() {
    let mut c = try_in_stored_arm_return(Ok(4), true);
    assert_eq!(c.start(), CoroutineState::Complete(Ok(5)));

    let mut c = try_in_stored_arm_return(Err(ParseError), true);
    assert_eq!(c.start(), CoroutineState::Complete(Err(ParseError)));

    let mut c = try_in_stored_arm_return(Ok(4), false);
    assert_eq!(c.start(), CoroutineState::Yielded(9));
    assert_eq!(c.resume(3), CoroutineState::Complete(Ok(6)));
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
