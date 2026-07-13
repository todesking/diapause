use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::analyze::{self, Analysis, VarDef};
use crate::args::MacroArgs;
use crate::parse::{self, CoroutineIr};

pub fn expand(attr: TokenStream, item: syn::ItemFn) -> syn::Result<TokenStream> {
    let macro_args: MacroArgs = syn::parse2(attr)?;

    check_signature(&item.sig)?;
    let args = parse_args(&item.sig)?;
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
        State::Start { #(#arg_pat),* } => {
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

    Ok(quote! {
        #(#attrs)*
        #vis fn #name(#(#arg_ident: #arg_ty),*) -> #name::State {
            #name::State::Start { #(#arg_ident),* }
        }

        #vis mod #name {
            #[allow(unused_imports)]
            use super::*;

            pub enum State {
                Start { #(#arg_ident: #arg_ty),* },
                #(#state_variants,)*
                Done,
                #poisoned_variant
            }

            impl ::baregen::Coroutine<#resume_ty> for State {
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
    if !sig.generics.params.is_empty() || sig.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(
            &sig.generics,
            "generic parameters are not supported yet",
        ));
    }
    Ok(())
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
            if let syn::Type::Reference(r) = &*pat_type.ty {
                return Err(syn::Error::new_spanned(
                    r,
                    "reference arguments are not supported yet",
                ));
            }
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
