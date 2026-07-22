//! Type parameters, where clauses, reference arguments (with lifetime
//! elision), and `impl Trait` arguments.

use std::fmt::Display;

use diapause::{Coroutine, CoroutineState};

#[diapause::coroutine(yield = String)]
fn show<T: Display>(x: T) -> String {
    yield_!(format!("first: {}", x));
    format!("last: {}", x)
}

#[test]
fn type_param_with_bound() {
    let mut c = show(42);
    assert_eq!(c.start(), CoroutineState::Yielded("first: 42".to_string()));
    assert_eq!(
        c.resume(()),
        CoroutineState::Complete("last: 42".to_string())
    );
}

#[diapause::coroutine(yield = u32)]
fn duplicate<T>(a: T) -> (T, T)
where
    T: Clone,
{
    yield_!(1);
    let b = a.clone();
    (a, b)
}

#[test]
fn where_clause_is_copied() {
    let mut c = duplicate("x".to_string());
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(
        c.resume(()),
        CoroutineState::Complete(("x".to_string(), "x".to_string()))
    );
}

#[diapause::coroutine(yield = i32)]
fn add_into(target: &mut Vec<i32>) {
    target.push(1);
    yield_!(1);
    target.push(2);
}

#[test]
fn elided_reference_arg_lives_across_yield() {
    let mut v = Vec::new();
    {
        let mut c = add_into(&mut v);
        assert_eq!(c.start(), CoroutineState::Yielded(1));
        assert_eq!(c.resume(()), CoroutineState::Complete(()));
    }
    assert_eq!(v, [1, 2]);
}

#[diapause::coroutine(yield = u32)]
fn tail<'x>(s: &'x str) -> &'x str {
    yield_!(1);
    &s[1..]
}

#[test]
fn named_lifetime_in_signature_and_return() {
    let mut c = tail("abc");
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete("bc"));
}

#[diapause::coroutine(yield = i32)]
fn apply(f: impl Fn(i32) -> i32) -> i32 {
    yield_!(1);
    f(41)
}

#[test]
fn impl_trait_arg_becomes_type_param() {
    let mut c = apply(|n| n + 1);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(42));
}

#[diapause::coroutine(yield = i32)]
fn make_default<T: Default + Into<i64>>() -> i64 {
    yield_!(1);
    let v: T = T::default();
    v.into()
}

#[test]
fn body_only_type_param_is_anchored_by_phantom() {
    let mut c = make_default::<u8>();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(0));
}

#[diapause::coroutine(yield = u32)]
fn mixed<'k, T: Display>(x: T, y: &'k mut String, z: &u8) -> usize {
    y.push_str(&format!("{}", x));
    yield_!(1);
    y.push('!');
    y.len() + (*z as usize)
}

#[test]
fn type_params_and_multiple_reference_args() {
    let mut s = String::from(">");
    let mut c = mixed(7, &mut s, &2);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(3 + 2));
    assert_eq!(s, ">7!");
}

// Type parameter appears in yield type
#[diapause::coroutine(yield = T)]
fn yield_type_param<T: Clone + Default>(x: T) -> T {
    yield_!(x.clone());
    x
}

#[test]
fn type_param_in_yield_type() {
    let mut c = yield_type_param(42i32);
    assert_eq!(c.start(), CoroutineState::Yielded(42));
    assert_eq!(c.resume(()), CoroutineState::Complete(42));

    let mut c = yield_type_param("hello".to_string());
    assert_eq!(c.start(), CoroutineState::Yielded("hello".to_string()));
    assert_eq!(c.resume(()), CoroutineState::Complete("hello".to_string()));
}

// Type parameter appears in return type
#[diapause::coroutine(yield = u32)]
fn return_type_param<T: Clone>(x: T, _y: T) -> T {
    yield_!(1);
    x
}

#[test]
fn type_param_in_return_type() {
    let mut c = return_type_param(42i32, 0i32);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(42));

    let mut c = return_type_param("test".to_string(), "".to_string());
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete("test".to_string()));
}

// Multiple type parameters
#[diapause::coroutine(yield = U)]
fn multiple_type_params<T: Display, U: Clone>(x: T, y: U) -> U {
    yield_!(y.clone());
    let _s = format!("{}", x);
    y
}

#[test]
fn multiple_type_params_in_function() {
    let mut c = multiple_type_params(42, "result".to_string());
    assert_eq!(c.start(), CoroutineState::Yielded("result".to_string()));
    assert_eq!(c.resume(()), CoroutineState::Complete("result".to_string()));
}

// Type parameter with multiple bounds
#[diapause::coroutine(yield = u32)]
fn multiple_bounds<T: Clone + Display + Default>() -> T {
    yield_!(1);
    T::default()
}

#[test]
fn type_param_with_multiple_bounds() {
    let mut c = multiple_bounds::<i32>();
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(0));
}

// Reference to slice
#[diapause::coroutine(yield = u32)]
fn slice_ref(items: &[i32]) -> usize {
    yield_!(1);
    items.len()
}

#[test]
fn reference_to_slice_argument() {
    let arr = [1, 2, 3, 4, 5];
    let mut c = slice_ref(&arr);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(5));
}

// Reference to &str
#[diapause::coroutine(yield = u32)]
fn str_ref(s: &str) -> usize {
    yield_!(1);
    s.len()
}

#[test]
fn reference_to_str_argument() {
    let mut c = str_ref("hello");
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(5));
}

// Mutable reference argument
#[diapause::coroutine(yield = u32)]
fn mutable_ref(vec: &mut Vec<u32>) -> u32 {
    yield_!(1);
    let sum: u32 = vec.iter().sum();
    vec.push(sum);
    sum
}

#[test]
fn mutable_reference_argument() {
    let mut v = vec![1, 2, 3];
    let mut c = mutable_ref(&mut v);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(6));
    assert_eq!(v, [1, 2, 3, 6]);
}

// Multiple references with different lifetimes
#[diapause::coroutine(yield = u32)]
fn multi_refs<'a, 'b>(s1: &'a str, s2: &'b str) -> usize {
    yield_!(1);
    s1.len() + s2.len()
}

#[test]
fn multiple_reference_args_with_different_lifetimes() {
    let s1 = "hello".to_string();
    let s2 = "world".to_string();
    let mut c = multi_refs(s1.as_str(), s2.as_str());
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(10));
}

// Slice with reference across yield
#[diapause::coroutine(yield = u32)]
fn slice_across_yield(items: &[u32]) -> u32 {
    yield_!(1);
    items.iter().sum()
}

#[test]
fn slice_reference_across_yield() {
    let arr = [10, 20, 30];
    let mut c = slice_across_yield(&arr);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(60));
}

// Multiple impl Trait arguments
#[diapause::coroutine(yield = i32)]
fn multiple_impl_trait(f: impl Fn(i32) -> i32, g: impl Fn(i32) -> i32) -> i32 {
    yield_!(1);
    f(g(41))
}

#[test]
fn multiple_impl_trait_args() {
    let mut c = multiple_impl_trait(|n| n + 1, |n| n * 2);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete((41 * 2) + 1));
}

// impl Trait with associated type
#[diapause::coroutine(yield = u32)]
fn impl_with_assoc_type(iter: impl Iterator<Item = u32>) -> u32 {
    yield_!(1);
    iter.sum()
}

#[test]
fn impl_trait_with_associated_type() {
    let vec = vec![1, 2, 3, 4, 5];
    let mut c = impl_with_assoc_type(vec.into_iter());
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(15));
}

// impl Trait + type parameter mixed
#[diapause::coroutine(yield = u32)]
fn impl_and_type_param<T: Clone>(x: T, f: impl Fn(T) -> T) -> T {
    yield_!(1);
    f(x)
}

#[test]
fn impl_trait_and_type_param_mixed() {
    let mut c = impl_and_type_param(42, |n| n + 1);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(43));

    let mut c = impl_and_type_param("x".to_string(), |s| s + "y");
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete("xy".to_string()));
}

// impl Trait with IntoIterator (owned)
#[diapause::coroutine(yield = u32)]
fn impl_into_iter(items: impl IntoIterator<Item = u32>) -> u32 {
    yield_!(1);
    items.into_iter().sum()
}

#[test]
fn impl_trait_with_into_iterator() {
    let vec = vec![1, 2, 3, 4, 5];
    let mut c = impl_into_iter(vec);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(15));

    let arr = [10, 20, 30];
    let mut c = impl_into_iter(arr);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(60));
}

// Const generics
#[diapause::coroutine(yield = u32)]
fn const_generic_array<const N: usize>(arr: [u32; N]) -> u32 {
    yield_!(1);
    arr.iter().sum()
}

#[test]
fn const_generic_parameter() {
    let mut c = const_generic_array([1, 2, 3, 4, 5]);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(15));

    let mut c = const_generic_array([10, 20]);
    assert_eq!(c.start(), CoroutineState::Yielded(1));
    assert_eq!(c.resume(()), CoroutineState::Complete(30));
}

// Generic inner with fixed yield type, used in yield_all! delegation
#[diapause::coroutine(yield = u32)]
fn inner_generic_u32<T: Clone>(_x: T) -> u32 {
    yield_!(100);
    200
}

#[diapause::coroutine(yield = u32)]
fn outer_delegates_to_generic<T: Clone>(x: T) -> u32 {
    let g: inner_generic_u32::State<T> = inner_generic_u32(x);
    yield_all!(g)
}

#[test]
fn generic_coroutine_with_yield_all() {
    let mut c = outer_delegates_to_generic("test".to_string());
    assert_eq!(c.start(), CoroutineState::Yielded(100));
    assert_eq!(c.resume(()), CoroutineState::Complete(200));

    let mut c = outer_delegates_to_generic(42i32);
    assert_eq!(c.start(), CoroutineState::Yielded(100));
    assert_eq!(c.resume(()), CoroutineState::Complete(200));
}
