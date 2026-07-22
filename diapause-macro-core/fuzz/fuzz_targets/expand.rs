#![no_main]

//! Fuzz target for `diapause_macro_core::expand`.
//!
//! Goal: any input must fail as a diagnostic (a `syn::Error` that the
//! real proc-macro turns into a compile error), never as a panic. The
//! input bytes are read as UTF-8 source of a single `fn`, parsed into a
//! `syn::ItemFn`, and driven through the same path the real macro and
//! the coverage harness use: extract the `#[diapause::coroutine(..)]`
//! attribute arguments (if present), attach the standard derives, and
//! call `expand`. Parse and expansion errors are expected and ignored;
//! only a panic is a finding.

use libfuzzer_sys::fuzz_target;
use proc_macro2::TokenStream;

fn try_expand(source: &str) {
    let Ok(mut item) = syn::parse_str::<syn::ItemFn>(source) else {
        return;
    };
    // Pull the coroutine attribute's argument tokens out of the parsed
    // function, mirroring `coverage_corpus::expand_case`. Without such
    // an attribute, expand with empty (all-default) arguments.
    let args = item
        .attrs
        .iter()
        .position(|a| {
            a.path()
                .segments
                .last()
                .is_some_and(|s| s.ident == "coroutine")
        })
        .map(|idx| {
            let attr = item.attrs.remove(idx);
            match attr.meta {
                syn::Meta::List(list) => list.tokens,
                _ => TokenStream::new(),
            }
        })
        .unwrap_or_default();

    // Derives arrive below the attribute in real expansions, so add
    // them here too to exercise the derive-forwarding paths.
    item.attrs
        .push(syn::parse_quote!(#[derive(Clone, serde::Serialize, serde::Deserialize)]));

    // The result (Ok codegen or Err diagnostic) is irrelevant; we only
    // care that this call returns rather than panics.
    let _ = diapause_macro_core::expand(args, item);
}

fuzz_target!(|data: &[u8]| {
    if let Ok(source) = std::str::from_utf8(data) {
        try_expand(source);
    }
});
