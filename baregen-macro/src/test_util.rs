//! Shared helpers for the `lower` and `analyze_cfg` unit test modules.

use crate::cfg::Cfg;
use crate::lower::lower;

/// Lowers `block` against argument names (idents built with call-site
/// spans), panicking on lowering errors.
pub(crate) fn lower_args(args: &[&str], block: &syn::Block) -> Cfg {
    let idents: Vec<syn::Ident> = args
        .iter()
        .map(|a| syn::Ident::new(a, proc_macro2::Span::call_site()))
        .collect();
    lower(&idents, block).unwrap()
}
