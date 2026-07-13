//! Procedural macro implementation for the `baregen` crate.
//!
//! Users should depend on `baregen`, which re-exports the attribute.

use proc_macro::TokenStream;

/// Transforms a function into a coroutine state machine.
///
/// See the `baregen` crate documentation for details.
#[proc_macro_attribute]
pub fn coroutine(_attr: TokenStream, item: TokenStream) -> TokenStream {
    // Task 01: pass the function through unchanged. The actual
    // transformation is implemented in later tasks.
    item
}
