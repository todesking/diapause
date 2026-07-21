//! Parsing and validation of the coroutine function's signature:
//! argument patterns, return-type checks, and rewriting elided
//! lifetimes / `impl Trait` into fresh generic parameters.

use std::collections::HashSet;

use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::visit_mut::VisitMut;

use crate::analyze_cfg::ArgInfo;
use crate::lower::{ErrorSink, collect_token_idents};

/// A function argument. Carries the `ident` needed to emit code, unlike
/// `analyze_cfg::ArgInfo`, which drops it because that crate identifies
/// arguments by `BindingId` instead. A non-simple-identifier pattern is
/// replaced by a fresh `__argN` ident (which is what the state stores)
/// and kept in `pattern`; the body then destructures it via a
/// synthesized `let <pattern> = __argN;` at the entry block.
pub(crate) struct ArgVar {
    pub(crate) ident: syn::Ident,
    pub(crate) mutability: Option<syn::Token![mut]>,
    pub(crate) ty: syn::Type,
    /// The original pattern, when it is not a simple identifier.
    pub(crate) pattern: Option<syn::Pat>,
}

impl From<&ArgVar> for ArgInfo {
    fn from(arg: &ArgVar) -> Self {
        ArgInfo {
            mutability: arg.mutability,
            ty: arg.ty.clone(),
        }
    }
}

pub(crate) fn check_signature(sig: &syn::Signature) -> syn::Result<()> {
    let unsupported = |span_source: &dyn quote::ToTokens, what: &str| {
        Err(syn::Error::new_spanned(
            span_source,
            format!("#[diapause::coroutine] does not support {what}"),
        ))
    };
    if let Some(c) = &sig.constness {
        return unsupported(c, "const functions");
    }
    if let Some(a) = &sig.asyncness {
        return unsupported(a, "async functions");
    }
    if let Some(u) = &sig.unsafety {
        return unsupported(u, "unsafe functions");
    }
    if let Some(abi) = &sig.abi {
        return unsupported(abi, "extern functions");
    }
    if let Some(v) = &sig.variadic {
        return unsupported(v, "variadic functions");
    }
    Ok(())
}

/// The return type becomes `type Return` of the impl, where lifetime
/// elision and `impl Trait` are not available.
pub(crate) fn check_return_type(ty: &syn::Type) -> syn::Result<()> {
    struct Check {
        error: ErrorSink,
    }
    impl Check {
        fn record(&mut self, e: syn::Error) {
            self.error.push(e);
        }
    }
    impl<'ast> Visit<'ast> for Check {
        fn visit_type_reference(&mut self, r: &'ast syn::TypeReference) {
            if r.lifetime.is_none() {
                self.record(syn::Error::new(
                    r.span(),
                    "elided lifetimes in the return type are not supported; \
                     use a named lifetime",
                ));
            }
            syn::visit::visit_type_reference(self, r);
        }
        fn visit_lifetime(&mut self, lt: &'ast syn::Lifetime) {
            if lt.ident == "_" {
                self.record(syn::Error::new(
                    lt.span(),
                    "elided lifetimes in the return type are not supported; \
                     use a named lifetime",
                ));
            }
        }
        fn visit_type_impl_trait(&mut self, it: &'ast syn::TypeImplTrait) {
            self.record(syn::Error::new(
                it.span(),
                "`impl Trait` in the return type is not supported",
            ));
        }
    }
    let mut check = Check {
        error: ErrorSink::default(),
    };
    check.visit_type(ty);
    check.error.into_result(())
}

pub(crate) fn parse_args(sig: &syn::Signature) -> syn::Result<Vec<ArgVar>> {
    let simple = |pat: &syn::Pat| match pat {
        syn::Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => Some(pi.clone()),
        _ => None,
    };
    // Names taken by simple-identifier arguments; the fresh `__argN`
    // names must not collide with them (they share the State enum's
    // Start-variant field namespace).
    let taken: HashSet<String> = sig
        .inputs
        .iter()
        .filter_map(|input| match input {
            syn::FnArg::Typed(pt) => simple(&pt.pat).map(|pi| pi.ident.to_string()),
            syn::FnArg::Receiver(_) => None,
        })
        .collect();
    let mut fresh = (0usize..)
        .map(|i| format!("__arg{i}"))
        .filter(|name| !taken.contains(name));
    sig.inputs
        .iter()
        .map(|input| {
            let pat_type = match input {
                syn::FnArg::Receiver(r) => {
                    return Err(syn::Error::new_spanned(
                        r,
                        "#[diapause::coroutine] cannot be applied to methods",
                    ));
                }
                syn::FnArg::Typed(pt) => pt,
            };
            let ty = (*pat_type.ty).clone();
            Ok(match simple(&pat_type.pat) {
                Some(pi) => ArgVar {
                    ident: pi.ident.clone(),
                    mutability: pi.mutability,
                    ty,
                    pattern: None,
                },
                // Any other pattern (destructuring, `_`, `ref`, `@`):
                // the state stores the value under a fresh name and the
                // entry block destructures it.
                None => ArgVar {
                    ident: syn::Ident::new(&fresh.next().unwrap(), pat_type.pat.span()),
                    mutability: None,
                    ty,
                    pattern: Some((*pat_type.pat).clone()),
                },
            })
        })
        .collect()
}

/// Rewrites argument types so they can appear on the State enum: elided
/// lifetimes get fresh named parameters and `impl Trait` becomes a named
/// type parameter. Returns the function's generics augmented with the
/// fresh parameters.
pub(crate) fn augment_generics(
    sig: &syn::Signature,
    args: &mut [ArgVar],
) -> syn::Result<syn::Generics> {
    let mut generics = sig.generics.clone();
    let mut rewriter = TypeRewriter {
        used_lifetimes: generics
            .lifetimes()
            .map(|l| l.lifetime.ident.to_string())
            .collect(),
        used_type_params: generics
            .type_params()
            .map(|t| t.ident.to_string())
            .collect(),
        fresh_lifetimes: Vec::new(),
        fresh_type_params: Vec::new(),
    };
    for arg in args.iter_mut() {
        rewriter.visit_type_mut(&mut arg.ty);
    }
    // Lifetime parameters must precede type parameters.
    for (i, lt) in rewriter.fresh_lifetimes.into_iter().enumerate() {
        generics
            .params
            .insert(i, syn::GenericParam::Lifetime(syn::LifetimeParam::new(lt)));
    }
    for tp in rewriter.fresh_type_params {
        generics.params.push(syn::GenericParam::Type(tp));
    }
    Ok(generics)
}

struct TypeRewriter {
    used_lifetimes: HashSet<String>,
    used_type_params: HashSet<String>,
    fresh_lifetimes: Vec<syn::Lifetime>,
    fresh_type_params: Vec<syn::TypeParam>,
}

impl TypeRewriter {
    fn fresh_lifetime(&mut self, span: proc_macro2::Span) -> syn::Lifetime {
        let name = ('a'..='z')
            .map(|c| c.to_string())
            .chain((0..).map(|i| format!("lt{i}")))
            .find(|name| !self.used_lifetimes.contains(name))
            .unwrap();
        self.used_lifetimes.insert(name.clone());
        let lt = syn::Lifetime::new(&format!("'{name}"), span);
        self.fresh_lifetimes.push(lt.clone());
        lt
    }
}

impl VisitMut for TypeRewriter {
    fn visit_type_reference_mut(&mut self, r: &mut syn::TypeReference) {
        if r.lifetime.is_none() {
            r.lifetime = Some(self.fresh_lifetime(r.and_token.span()));
        }
        syn::visit_mut::visit_type_reference_mut(self, r);
    }

    fn visit_lifetime_mut(&mut self, lt: &mut syn::Lifetime) {
        if lt.ident == "_" {
            *lt = self.fresh_lifetime(lt.span());
        }
    }

    fn visit_type_mut(&mut self, ty: &mut syn::Type) {
        syn::visit_mut::visit_type_mut(self, ty);
        if let syn::Type::ImplTrait(it) = ty {
            let name = (0..)
                .map(|i| format!("__T{i}"))
                .find(|name| !self.used_type_params.contains(name))
                .unwrap();
            self.used_type_params.insert(name.clone());
            let ident = syn::Ident::new(&name, it.span());
            let bounds = it.bounds.clone();
            self.fresh_type_params
                .push(syn::parse_quote!(#ident: #bounds));
            *ty = syn::parse_quote!(#ident);
        }
    }
}

/// Returns the PhantomData payload type anchoring generic parameters that
/// appear in no variant field, or `None` if every parameter is used.
///
/// Detection is a token-level ident scan of the field types: a false
/// "used" only omits the phantom and surfaces as E0392.
pub(crate) fn phantom_for_unused_params<'a>(
    generics: &syn::Generics,
    field_tys: impl Iterator<Item = &'a syn::Type>,
) -> Option<syn::Type> {
    use quote::ToTokens;

    let mut used = HashSet::new();
    for ty in field_tys {
        collect_token_idents(ty.to_token_stream(), &mut used);
    }

    let unused_types: Vec<&syn::Ident> = generics
        .type_params()
        .map(|t| &t.ident)
        .filter(|id| !used.contains(&id.to_string()))
        .collect();
    let unused_lifetimes: Vec<&syn::Lifetime> = generics
        .lifetimes()
        .map(|l| &l.lifetime)
        .filter(|lt| !used.contains(&lt.ident.to_string()))
        .collect();
    if unused_types.is_empty() && unused_lifetimes.is_empty() {
        return None;
    }
    // fn pointers keep the phantom covariant and unconditionally
    // Send/Sync/Copy/Clone.
    Some(syn::parse_quote!((#(fn() -> #unused_types,)* #(fn() -> &#unused_lifetimes (),)*)))
}

#[cfg(test)]
mod tests {
    use proc_macro2::TokenStream;
    use syn::parse_quote;

    fn expand_err(item: syn::ItemFn) -> syn::Error {
        crate::expand::expand(TokenStream::new(), item).unwrap_err()
    }

    fn expand_err_msg(item: syn::ItemFn) -> String {
        expand_err(item).to_string()
    }

    #[test]
    fn const_functions_are_rejected() {
        let msg = expand_err_msg(parse_quote!(
            const fn c() {}
        ));
        assert!(
            msg.contains("does not support const functions"),
            "got: {msg}"
        );
    }

    #[test]
    fn async_functions_are_rejected() {
        let msg = expand_err_msg(parse_quote!(
            async fn c() {}
        ));
        assert!(
            msg.contains("does not support async functions"),
            "got: {msg}"
        );
    }

    #[test]
    fn unsafe_functions_are_rejected() {
        let msg = expand_err_msg(parse_quote!(
            unsafe fn c() {}
        ));
        assert!(
            msg.contains("does not support unsafe functions"),
            "got: {msg}"
        );
    }

    #[test]
    fn extern_functions_are_rejected() {
        let msg = expand_err_msg(parse_quote!(
            extern "C" fn c() {}
        ));
        assert!(
            msg.contains("does not support extern functions"),
            "got: {msg}"
        );
    }

    #[test]
    fn variadic_functions_are_rejected() {
        // A plain variadic fn is syntactically valid to syn (rustc
        // rejects it semantically); an `extern` variadic would hit the
        // abi check first.
        let msg = expand_err_msg(parse_quote!(
            fn c(x: u32, ...) {}
        ));
        assert!(
            msg.contains("does not support variadic functions"),
            "got: {msg}"
        );
    }

    #[test]
    fn methods_are_rejected() {
        let msg = expand_err_msg(parse_quote!(
            fn c(&self) {}
        ));
        assert!(msg.contains("cannot be applied to methods"), "got: {msg}");
    }

    #[test]
    fn elided_reference_lifetime_in_return_type_is_rejected() {
        let msg = expand_err_msg(parse_quote!(
            fn c(x: &'static u32) -> &u32 {
                x
            }
        ));
        assert!(
            msg.contains("elided lifetimes in the return type"),
            "got: {msg}"
        );
        assert!(msg.contains("use a named lifetime"), "got: {msg}");
    }

    #[test]
    fn underscore_lifetime_in_return_type_is_rejected() {
        let msg = expand_err_msg(parse_quote!(
            fn c() -> Wrapper<'_> {
                todo!()
            }
        ));
        assert!(
            msg.contains("elided lifetimes in the return type"),
            "got: {msg}"
        );
    }

    #[test]
    fn impl_trait_in_return_type_is_rejected() {
        let msg = expand_err_msg(parse_quote!(
            fn c() -> impl Iterator<Item = u32> {
                todo!()
            }
        ));
        assert!(
            msg.contains("`impl Trait` in the return type is not supported"),
            "got: {msg}"
        );
    }

    #[test]
    fn return_type_errors_are_combined() {
        let err = expand_err(parse_quote!(
            fn c() -> (&u32, impl Sized) {
                todo!()
            }
        ));
        assert_eq!(err.into_iter().count(), 2);
    }
}
