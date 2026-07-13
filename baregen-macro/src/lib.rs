//! Procedural macro implementation for the `baregen` crate.
//!
//! Users should depend on `baregen`, which re-exports the attribute.

use proc_macro::TokenStream;

mod args;
mod expand;
mod parse;

/// Transforms a function into a coroutine state machine.
///
/// See the `baregen` crate documentation for details.
#[proc_macro_attribute]
pub fn coroutine(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = syn::parse_macro_input!(item as syn::ItemFn);
    expand::expand(attr.into(), item)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
