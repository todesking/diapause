//! Syntactic type inference and borrow classification for `let` binding
//! initializers. Purely a function of the initializer's surface syntax
//! and the bindings already in scope — no type-checking is involved.

use crate::cfg::{BorrowSource, TySource};
use crate::lower::Lowerer;

impl Lowerer {
    /// Syntactic type inference for an initializer expression.
    pub(crate) fn infer_ty_source(&self, expr: &syn::Expr) -> TySource {
        match strip_parens(expr) {
            // Negation of a suffixed numeric literal keeps its type.
            syn::Expr::Unary(u) if matches!(u.op, syn::UnOp::Neg(_)) => match &*u.expr {
                syn::Expr::Lit(_) => self.infer_ty_source(&u.expr),
                _ => TySource::Unknown,
            },
            syn::Expr::Lit(lit) => infer_lit_ty(&lit.lit).map_or(TySource::Unknown, TySource::Known),
            // Move propagation: `let y = x;` follows x's type.
            syn::Expr::Path(p) if p.qself.is_none() => p
                .path
                .get_ident()
                .and_then(|i| self.resolve(&i.to_string()))
                .map_or(TySource::Unknown, TySource::Moved),
            syn::Expr::Range(r) => match (&r.start, &r.end) {
                (Some(start), Some(end)) => TySource::Range {
                    inclusive: matches!(r.limits, syn::RangeLimits::Closed(_)),
                    start: Box::new(self.infer_ty_source(start)),
                    end: Box::new(self.infer_ty_source(end)),
                },
                _ => TySource::Unknown,
            },
            _ => TySource::Unknown,
        }
    }

    pub(crate) fn classify_borrow(
        &self,
        init: Option<&syn::Expr>,
        annotated: Option<&syn::Type>,
    ) -> BorrowSource {
        let init = init.map(strip_parens);
        match init {
            Some(syn::Expr::Reference(r)) => match &*r.expr {
                syn::Expr::Path(p) if p.qself.is_none() && p.path.get_ident().is_some() => {
                    let source_ident = p.path.get_ident().unwrap().clone();
                    BorrowSource::Direct {
                        source: self.resolve(&source_ident.to_string()),
                        source_ident,
                        mutable: r.mutability.is_some(),
                    }
                }
                _ => BorrowSource::NonReconstructible {
                    why: "is held across yield_! but borrows a non-trivial place; only direct \
                          borrows of local variables (`let y = &x;` / `let y = &mut x;`) can be \
                          reconstructed after resume",
                },
            },
            _ if matches!(annotated, Some(syn::Type::Reference(_))) => {
                BorrowSource::NonReconstructible {
                    why: "has a reference type but is not a direct borrow (`let y = &x;` / \
                          `let y = &mut x;`), so it cannot be held across yield_!",
                }
            }
            _ => BorrowSource::NotABorrow,
        }
    }
}

/// Unwraps nested `(expr)` parenthesization down to the innermost expression.
pub(crate) fn strip_parens(expr: &syn::Expr) -> &syn::Expr {
    let mut expr = expr;
    while let syn::Expr::Paren(p) = expr {
        expr = &p.expr;
    }
    expr
}

/// The manifest type of a literal: an explicit suffix (`123u8`, `1.5f32`)
/// or an unambiguous literal kind (`true`, `'c'`, `b'x'`). Unsuffixed
/// numeric literals are NOT given the i32/f64 default: the actual type
/// may be inferred differently by rustc, and guessing wrong would surface
/// as a confusing error in generated code.
fn infer_lit_ty(lit: &syn::Lit) -> Option<syn::Type> {
    let suffix_ty = |suffix: &str, span: proc_macro2::Span| -> Option<syn::Type> {
        if suffix.is_empty() {
            return None;
        }
        let ident = syn::Ident::new(suffix, span);
        Some(syn::parse_quote!(#ident))
    };
    match lit {
        syn::Lit::Int(i) => suffix_ty(i.suffix(), i.span()),
        syn::Lit::Float(f) => suffix_ty(f.suffix(), f.span()),
        syn::Lit::Bool(_) => Some(syn::parse_quote!(bool)),
        syn::Lit::Char(_) => Some(syn::parse_quote!(char)),
        syn::Lit::Byte(_) => Some(syn::parse_quote!(u8)),
        _ => None,
    }
}
