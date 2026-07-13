use std::collections::HashSet;

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::visit_mut::VisitMut;

use crate::analyze::{self, Analysis, VarDef};
use crate::args::MacroArgs;
use crate::parse::{self, CoroutineIr};

pub fn expand(attr: TokenStream, item: syn::ItemFn) -> syn::Result<TokenStream> {
    let macro_args: MacroArgs = syn::parse2(attr)?;

    check_signature(&item.sig)?;
    let mut args = parse_args(&item.sig)?;
    let generics = augment_generics(&item.sig, &mut args)?;
    let ir = parse::parse_body(&item.block)?;
    let analysis = analyze::analyze(&args, &ir, &macro_args.resume_ty)?;
    let states = &analysis.states;

    let attrs = &item.attrs;
    let vis = &item.vis;
    let name = &item.sig.ident;
    let yield_ty = &macro_args.yield_ty;
    let resume_ty = &macro_args.resume_ty;
    let ret_ty: syn::Type = match &item.sig.output {
        syn::ReturnType::Default => syn::parse_quote!(()),
        syn::ReturnType::Type(_, ty) => (**ty).clone(),
    };
    check_return_type(&ret_ty)?;

    let n = ir.yields.len();
    let state_idents: Vec<syn::Ident> = (1..=n).map(|k| format_ident!("S{k}")).collect();

    let arg_ident: Vec<_> = args.iter().map(|a| &a.ident).collect();
    let arg_ty: Vec<_> = args.iter().map(|a| a.ty.as_ref().unwrap()).collect();

    let state_variants = states.iter().zip(&state_idents).map(|(fields, id)| {
        let field_defs = fields.iter().map(|f| {
            let ident = &f.ident;
            let ty = &f.ty;
            quote!(#ident: #ty)
        });
        quote!(#id { #(#field_defs),* })
    });

    // A generic parameter used only inside the body would leave the enum
    // with an unconstrained parameter (E0392); a PhantomData field in
    // Start keeps such parameters anchored.
    let all_field_tys = arg_ty
        .iter()
        .copied()
        .chain(states.iter().flatten().map(|f| &f.ty));
    let phantom_ty = phantom_for_unused_params(&generics, all_field_tys);
    let phantom_field = phantom_ty
        .as_ref()
        .map(|ty| quote!(__phantom: ::core::marker::PhantomData<#ty>,));
    let phantom_init = phantom_ty
        .as_ref()
        .map(|_| quote!(__phantom: ::core::marker::PhantomData,));

    // Without yields no transition can panic halfway, so Done doubles as
    // the placeholder and Poisoned is omitted.
    let (poisoned_variant, placeholder) = if n == 0 {
        (quote!(), quote!(State::Done))
    } else {
        (quote!(Poisoned,), quote!(State::Poisoned))
    };

    let start_body = segment_code(&ir, &analysis, &state_idents, 0, yield_ty, &ret_ty);
    let arg_pat: Vec<_> = args
        .iter()
        .map(|a| bind_pat(&a.mutability, &a.ident))
        .collect();
    let start_arm = quote! {
        State::Start { #(#arg_pat,)* .. } => {
            #start_body
        }
    };

    let resume_arms = (1..=n).map(|k| {
        let state_ident = &state_idents[k - 1];
        let field_pats: Vec<_> = states[k - 1]
            .iter()
            .map(|f| bind_pat(&f.mutability, &f.ident))
            .collect();
        let resume_stmt = ir.yields[k - 1].resume_binding.as_ref().map(|rb| {
            let mutability = &rb.mutability;
            let ident = &rb.ident;
            let ty = rb.ty.as_ref().map(|ty| quote!(: #ty));
            quote!(let #mutability #ident #ty = _resume;)
        });
        let body = segment_code(&ir, &analysis, &state_idents, k, yield_ty, &ret_ty);
        quote! {
            State::#state_ident { #(#field_pats),* } => {
                #resume_stmt
                #body
            }
        }
    });

    let poisoned_arm = (n > 0).then(|| quote!(State::Poisoned => ::core::panic!("Poisoned"),));

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    Ok(quote! {
        #(#attrs)*
        #vis fn #name #impl_generics (#(#arg_ident: #arg_ty),*) -> #name::State #ty_generics
        #where_clause
        {
            #name::State::Start { #(#arg_ident,)* #phantom_init }
        }

        #vis mod #name {
            #[allow(unused_imports)]
            use super::*;

            pub enum State #generics #where_clause {
                Start { #(#arg_ident: #arg_ty,)* #phantom_field },
                #(#state_variants,)*
                Done,
                #poisoned_variant
            }

            impl #impl_generics ::baregen::Coroutine<#resume_ty> for State #ty_generics
            #where_clause
            {
                type Yield = #yield_ty;
                type Return = #ret_ty;

                #[allow(unused_mut)]
                fn start(&mut self) -> ::baregen::CoroutineState<#yield_ty, #ret_ty> {
                    // Fields must be moved out through &mut self, so the
                    // state is swapped for a placeholder up front.
                    match ::core::mem::replace(self, #placeholder) {
                        #start_arm
                        _ => ::core::panic!("Already started"),
                    }
                }

                #[allow(unused_mut)]
                fn resume(
                    &mut self,
                    _resume: #resume_ty,
                ) -> ::baregen::CoroutineState<#yield_ty, #ret_ty> {
                    match ::core::mem::replace(self, #placeholder) {
                        #(#resume_arms)*
                        State::Start { .. } => ::core::panic!("Not started"),
                        State::Done => ::core::panic!("Already done"),
                        #poisoned_arm
                    }
                }
            }
        }
    })
}

/// Field shorthand with the original binding mode: `mut x` rebinds the
/// stored variable mutably when the state is unpacked.
fn bind_pat(mutability: &Option<syn::Token![mut]>, ident: &syn::Ident) -> TokenStream {
    quote!(#mutability #ident)
}

/// Emits the code that runs segment `k` and either suspends at yield `k`
/// or completes the coroutine.
///
/// Borrows that crossed a yield into this segment are re-established
/// first; original borrow `let`s whose binding is only needed after a
/// yield are omitted (the analysis marked them as removed).
fn segment_code(
    ir: &CoroutineIr,
    analysis: &Analysis,
    state_idents: &[syn::Ident],
    k: usize,
    yield_ty: &syn::Type,
    ret_ty: &syn::Type,
) -> TokenStream {
    let reborrows = analysis.reborrows[k].iter().map(|rb| {
        let target_mut = &rb.target_mut;
        let target = &rb.target;
        let source = &rb.source;
        let mut_tok = rb.mutable.then(|| quote!(mut));
        quote!(let #target_mut #target = & #mut_tok #source;)
    });
    let stmts = ir.segments[k]
        .stmts
        .iter()
        .enumerate()
        .filter(|(i, _)| !analysis.removed_stmts[k].contains(i))
        .map(|(_, stmt)| stmt);
    if k < ir.yields.len() {
        let value = &ir.yields[k].value;
        let next = &state_idents[k];
        let field_idents = analysis.states[k].iter().map(|f| &f.ident);
        // The yield value is evaluated before live variables are moved
        // into the state, matching the original evaluation order.
        quote! {
            #(#reborrows)*
            #(#stmts)*
            let __yielded: #yield_ty = #value;
            *self = State::#next { #(#field_idents),* };
            ::baregen::CoroutineState::Yielded(__yielded)
        }
    } else {
        quote! {
            let __ret: #ret_ty = {
                #(#reborrows)*
                #(#stmts)*
            };
            *self = State::Done;
            ::baregen::CoroutineState::Complete(__ret)
        }
    }
}

fn check_signature(sig: &syn::Signature) -> syn::Result<()> {
    let unsupported = |span_source: &dyn quote::ToTokens, what: &str| {
        Err(syn::Error::new_spanned(
            span_source,
            format!("#[baregen::coroutine] does not support {what}"),
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
fn check_return_type(ty: &syn::Type) -> syn::Result<()> {
    struct Check {
        error: Option<syn::Error>,
    }
    impl Check {
        fn record(&mut self, e: syn::Error) {
            match &mut self.error {
                Some(prev) => prev.combine(e),
                None => self.error = Some(e),
            }
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
    let mut check = Check { error: None };
    check.visit_type(ty);
    match check.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn parse_args(sig: &syn::Signature) -> syn::Result<Vec<VarDef>> {
    sig.inputs
        .iter()
        .map(|input| {
            let pat_type = match input {
                syn::FnArg::Receiver(r) => {
                    return Err(syn::Error::new_spanned(
                        r,
                        "#[baregen::coroutine] cannot be applied to methods",
                    ));
                }
                syn::FnArg::Typed(pt) => pt,
            };
            match &*pat_type.pat {
                syn::Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => Ok(VarDef {
                    ident: pi.ident.clone(),
                    mutability: pi.mutability,
                    ty: Some((*pat_type.ty).clone()),
                }),
                other => Err(syn::Error::new_spanned(
                    other,
                    "argument patterns must be simple identifiers",
                )),
            }
        })
        .collect()
}

/// Rewrites argument types so they can appear on the State enum: elided
/// lifetimes get fresh named parameters and `impl Trait` becomes a named
/// type parameter. Returns the function's generics augmented with the
/// fresh parameters.
fn augment_generics(sig: &syn::Signature, args: &mut [VarDef]) -> syn::Result<syn::Generics> {
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
        if let Some(ty) = &mut arg.ty {
            rewriter.visit_type_mut(ty);
        }
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
            self.fresh_type_params.push(syn::parse_quote!(#ident: #bounds));
            *ty = syn::parse_quote!(#ident);
        }
    }
}

/// Returns the PhantomData payload type anchoring generic parameters that
/// appear in no variant field, or `None` if every parameter is used.
///
/// Detection is a token-level ident scan of the field types: a false
/// "used" only omits the phantom and surfaces as E0392.
fn phantom_for_unused_params<'a>(
    generics: &syn::Generics,
    field_tys: impl Iterator<Item = &'a syn::Type>,
) -> Option<syn::Type> {
    use quote::ToTokens;

    let mut used = HashSet::new();
    for ty in field_tys {
        collect_idents(ty.to_token_stream(), &mut used);
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

fn collect_idents(tokens: TokenStream, out: &mut HashSet<String>) {
    for tt in tokens {
        match tt {
            proc_macro2::TokenTree::Ident(id) => {
                out.insert(id.to_string());
            }
            proc_macro2::TokenTree::Group(g) => collect_idents(g.stream(), out),
            _ => {}
        }
    }
}
