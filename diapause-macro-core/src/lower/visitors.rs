//! Yield detection and the small AST visitors shared by lowering:
//! macro recognizers, the nested-scope skipping rule, and use/binding
//! collectors.

use std::collections::HashSet;

use syn::spanned::Spanned;
use syn::visit::Visit;

use super::{ERR_FOREIGN_MACRO, ERR_YIELD_ALL_POSITION, ERR_YIELD_ALL_RESUME_POSITION, ErrorSink};

pub fn is_yield_macro(mac: &syn::Macro) -> bool {
    mac.path
        .segments
        .last()
        .is_some_and(|seg| seg.ident == "yield_")
}

pub fn is_yield_all_macro(mac: &syn::Macro) -> bool {
    mac.path
        .segments
        .last()
        .is_some_and(|seg| seg.ident == "yield_all")
}

pub fn is_yield_all_resume_macro(mac: &syn::Macro) -> bool {
    mac.path
        .segments
        .last()
        .is_some_and(|seg| seg.ident == "yield_all_resume")
}

/// `yield_all!` or `yield_all_resume!` — the two delegation macros,
/// which share their supported positions and lowering.
pub fn is_delegate_macro(mac: &syn::Macro) -> bool {
    is_yield_all_macro(mac) || is_yield_all_resume_macro(mac)
}

/// The internal marker macro `rewrite_opaque_jumps` leaves where an
/// opaque-statement `break`/`continue` was: `__diapause_jump!(k)` (or
/// `__diapause_jump!(k, value)` for a completing valued `break`), where
/// `k` indexes `Cfg::opaque_jumps`. `expand` replaces the markers with
/// the real transitions once variant fields are known.
pub fn is_jump_marker(mac: &syn::Macro) -> bool {
    mac.path.is_ident("__diapause_jump")
}

/// Collects the `k` arguments of every `__diapause_jump!(k [, value])`
/// marker in a token stream, including markers nested inside another
/// marker's value.
pub(crate) fn collect_markers(tokens: proc_macro2::TokenStream, out: &mut Vec<usize>) {
    let mut iter = tokens.into_iter().peekable();
    while let Some(tt) = iter.next() {
        match tt {
            proc_macro2::TokenTree::Ident(id) if id == "__diapause_jump" => {
                if !matches!(
                    iter.peek(),
                    Some(proc_macro2::TokenTree::Punct(p)) if p.as_char() == '!'
                ) {
                    continue;
                }
                iter.next(); // the `!`
                if let Some(proc_macro2::TokenTree::Group(g)) = iter.next() {
                    let mut inner = g.stream().into_iter();
                    if let Some(proc_macro2::TokenTree::Literal(l)) = inner.next()
                        && let Ok(k) = l.to_string().parse::<usize>()
                    {
                        out.push(k);
                    }
                    // A completion marker's value may contain further
                    // (already rewritten) markers.
                    collect_markers(inner.collect(), out);
                }
            }
            proc_macro2::TokenTree::Group(g) => collect_markers(g.stream(), out),
            _ => {}
        }
    }
}

/// Textually scans a foreign macro's tokens for `yield_ !` /
/// `yield_all !` / `yield_all_resume !`.
pub(super) fn tokens_contain_yield(tokens: proc_macro2::TokenStream) -> bool {
    let mut iter = tokens.into_iter().peekable();
    while let Some(tt) = iter.next() {
        match tt {
            proc_macro2::TokenTree::Ident(id)
                if id == "yield_" || id == "yield_all" || id == "yield_all_resume" =>
            {
                if matches!(
                    iter.peek(),
                    Some(proc_macro2::TokenTree::Punct(p)) if p.as_char() == '!'
                ) {
                    return true;
                }
            }
            proc_macro2::TokenTree::Group(g) if tokens_contain_yield(g.stream()) => {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// Skips closures, async blocks, and nested items when visiting a
/// coroutine body: they are separate scopes, so `yield_!`, `?`, and
/// break/continue inside them must not be attributed to the enclosing
/// coroutine. Works for both `syn::visit::Visit` and
/// `syn::visit_mut::VisitMut` impls.
macro_rules! skip_nested_scopes {
    (Visit) => {
        fn visit_expr_closure(&mut self, _: &'ast syn::ExprClosure) {}
        fn visit_expr_async(&mut self, _: &'ast syn::ExprAsync) {}
        fn visit_item(&mut self, _: &'ast syn::Item) {}
    };
    (VisitMut) => {
        fn visit_expr_closure_mut(&mut self, _: &mut syn::ExprClosure) {}
        fn visit_expr_async_mut(&mut self, _: &mut syn::ExprAsync) {}
        fn visit_item_mut(&mut self, _: &mut syn::Item) {}
    };
}
pub(crate) use skip_nested_scopes;

/// Finds genuine `yield_!` / `yield_all!` / `yield_all_resume!`
/// invocations (the latter two desugar into yields). Closures, async
/// blocks, and nested items are separate scopes and pass through.
/// Foreign macros whose tokens mention yield_! do not count as
/// containing a yield; they are rejected separately.
#[derive(Default)]
struct ContainsYield {
    found: bool,
}

impl<'ast> Visit<'ast> for ContainsYield {
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if is_yield_macro(mac) || is_delegate_macro(mac) {
            self.found = true;
        }
    }

    skip_nested_scopes!(Visit);
}

pub(super) fn contains_yield_stmt(stmt: &syn::Stmt) -> bool {
    let mut c = ContainsYield::default();
    c.visit_stmt(stmt);
    c.found
}

pub(super) fn contains_yield_expr(expr: &syn::Expr) -> bool {
    let mut c = ContainsYield::default();
    c.visit_expr(expr);
    c.found
}

/// Reports every `yield_!` with a position-specific message and every
/// foreign macro carrying yield_! tokens.
pub(super) struct YieldBan<'a> {
    pub(super) msg: &'a str,
    pub(super) error: ErrorSink,
}

impl YieldBan<'_> {
    fn record(&mut self, e: syn::Error) {
        self.error.push(e);
    }
}

impl<'ast> Visit<'ast> for YieldBan<'_> {
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if is_yield_macro(mac) {
            self.record(syn::Error::new_spanned(mac, self.msg));
        } else if is_yield_all_macro(mac) {
            // Delegation macros are never hoisted, so the
            // position-specific hoisting advice does not apply; name
            // their own rules.
            self.record(syn::Error::new_spanned(mac, ERR_YIELD_ALL_POSITION));
        } else if is_yield_all_resume_macro(mac) {
            self.record(syn::Error::new_spanned(mac, ERR_YIELD_ALL_RESUME_POSITION));
        } else if tokens_contain_yield(mac.tokens.clone()) {
            self.record(syn::Error::new(mac.span(), ERR_FOREIGN_MACRO));
        }
    }

    skip_nested_scopes!(Visit);
}

// === Small collectors ===

/// Collects identifiers that may refer to local variables. Overapproximates:
/// every unqualified single-segment
/// path counts, and all identifiers inside macro invocations are taken
/// verbatim from the token stream.
#[derive(Default)]
pub(crate) struct UseCollector {
    pub(crate) used: HashSet<String>,
}

impl<'ast> Visit<'ast> for UseCollector {
    fn visit_path(&mut self, path: &'ast syn::Path) {
        if path.leading_colon.is_none() && path.segments.len() == 1 {
            self.used.insert(path.segments[0].ident.to_string());
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        collect_token_idents(mac.tokens.clone(), &mut self.used);
        syn::visit::visit_macro(self, mac);
    }
}

pub(crate) fn collect_token_idents(tokens: proc_macro2::TokenStream, out: &mut HashSet<String>) {
    for tt in tokens {
        match tt {
            proc_macro2::TokenTree::Ident(id) => {
                out.insert(id.to_string());
            }
            proc_macro2::TokenTree::Group(g) => collect_token_idents(g.stream(), out),
            _ => {}
        }
    }
}

/// Collects the identifiers a pattern binds, in visit order.
#[derive(Default)]
pub(crate) struct PatBindingCollector {
    pub(crate) bindings: Vec<(syn::Ident, Option<syn::Token![mut]>)>,
}

impl<'ast> Visit<'ast> for PatBindingCollector {
    fn visit_pat_ident(&mut self, pi: &'ast syn::PatIdent) {
        self.bindings.push((pi.ident.clone(), pi.mutability));
        syn::visit::visit_pat_ident(self, pi);
    }
}
