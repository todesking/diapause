//! Procedural macro implementation for the `baregen` crate.
//!
//! Users should depend on `baregen`, which re-exports the attribute.

use proc_macro::TokenStream;

mod analyze_cfg;
mod args;
mod expand;
mod lower;

/// Transforms a function into a coroutine state machine.
///
/// ```ignore
/// #[baregen::coroutine(yield = u32, resume = String)]
/// fn foo(a: u32) -> usize {
///     let r = yield_!(a + 1);
///     r.len()
/// }
/// ```
///
/// The attribute generates, in place of the function:
///
/// - a starter fn with the same name, visibility, and arguments that
///   returns the initial state (`foo::State`) without running any code;
/// - a module named after the function containing the `State` enum
///   (`Start`, one variant per suspension point, `Done`, and a
///   `Poisoned` placeholder) and its `Coroutine` implementation.
///
/// Attribute arguments `yield = Type` and `resume = Type` set the
/// yielded and resume-argument types; both default to `()`. The
/// `Return` type is the function's return type.
///
/// `#[derive(...)]` attributes written **below** this attribute are
/// moved onto the generated `State` enum; other attributes (doc
/// comments etc.) stay on the starter fn.
///
/// See the `baregen` crate documentation for the supported subset of
/// Rust and the diagnostics emitted for unsupported constructs.
#[proc_macro_attribute]
pub fn coroutine(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = syn::parse_macro_input!(item as syn::ItemFn);
    expand::expand(attr.into(), item)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
