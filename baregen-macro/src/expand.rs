use proc_macro2::TokenStream;
use quote::quote;

struct Arg {
    ident: syn::Ident,
    mutability: Option<syn::Token![mut]>,
    ty: syn::Type,
}

pub fn expand(attr: TokenStream, item: syn::ItemFn) -> syn::Result<TokenStream> {
    if !attr.is_empty() {
        return Err(syn::Error::new_spanned(
            attr,
            "attribute arguments are not supported yet",
        ));
    }

    check_signature(&item.sig)?;
    let args = parse_args(&item.sig)?;

    let attrs = &item.attrs;
    let vis = &item.vis;
    let name = &item.sig.ident;
    let body = &item.block;

    let ret_ty: syn::Type = match &item.sig.output {
        syn::ReturnType::Default => syn::parse_quote!(()),
        syn::ReturnType::Type(_, ty) => (**ty).clone(),
    };

    let arg_ident: Vec<_> = args.iter().map(|a| &a.ident).collect();
    let arg_ty: Vec<_> = args.iter().map(|a| &a.ty).collect();
    // Field shorthand with the original binding mode: `mut a` rebinds the
    // argument mutably when the state is unpacked in start().
    let arg_pat: Vec<_> = args
        .iter()
        .map(|a| {
            let mutability = &a.mutability;
            let ident = &a.ident;
            quote!(#mutability #ident)
        })
        .collect();

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
                Done,
            }

            impl ::baregen::Coroutine<()> for State {
                type Yield = ();
                type Return = #ret_ty;

                fn start(&mut self) -> ::baregen::CoroutineState<(), #ret_ty> {
                    // Fields must be moved out through &mut self, so the
                    // state is swapped for the terminal variant up front.
                    match ::core::mem::replace(self, State::Done) {
                        State::Start { #(#arg_pat),* } => {
                            let __ret: #ret_ty = #body;
                            ::baregen::CoroutineState::Complete(__ret)
                        }
                        State::Done => ::core::panic!("Already started"),
                    }
                }

                fn resume(&mut self, _resume: ()) -> ::baregen::CoroutineState<(), #ret_ty> {
                    match self {
                        State::Start { .. } => ::core::panic!("Not started"),
                        State::Done => ::core::panic!("Already done"),
                    }
                }
            }
        }
    })
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

fn parse_args(sig: &syn::Signature) -> syn::Result<Vec<Arg>> {
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
                syn::Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => Ok(Arg {
                    ident: pi.ident.clone(),
                    mutability: pi.mutability,
                    ty: (*pat_type.ty).clone(),
                }),
                other => Err(syn::Error::new_spanned(
                    other,
                    "argument patterns must be simple identifiers",
                )),
            }
        })
        .collect()
}
