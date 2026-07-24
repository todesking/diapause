use syn::Token;
use syn::ext::IdentExt;
use syn::parse::{Parse, ParseStream};

/// Arguments of `#[diapause::coroutine(yield = Type, resume = Type,
/// fingerprint, in_place = bool)]`. The types default to `()`.
pub struct MacroArgs {
    pub yield_ty: syn::Type,
    pub resume_ty: syn::Type,
    pub fingerprint: Fingerprint,
    /// The `in_place` argument (default `true`): whether eligible
    /// resume arms may update the suspended state in place instead of
    /// moving it out and back. `in_place = false` restores the
    /// unconditional panics-leave-`Poisoned` guarantee (see
    /// `analyze_cfg::InPlacePlan`).
    pub in_place: bool,
}

/// The `fingerprint` argument: when given, the `__fp` field and its
/// checks are generated. Bare `fingerprint` hashes the coroutine's
/// source; `fingerprint = "tag"` hashes the tag instead — an escape
/// hatch declaring states persisted under the same tag compatible
/// across an edit (e.g. resuming states persisted before a hot fix).
pub enum Fingerprint {
    Off,
    FromSource,
    Manual(syn::LitStr),
}

impl Fingerprint {
    pub fn enabled(&self) -> bool {
        !matches!(self, Fingerprint::Off)
    }
}

impl Parse for MacroArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut yield_ty = None;
        let mut resume_ty = None;
        let mut fingerprint = None;
        let mut in_place = None;
        while !input.is_empty() {
            // `yield` is a reserved keyword, so accept any identifier here.
            let name = input.call(syn::Ident::parse_any)?;
            let duplicate = |name: &syn::Ident| {
                syn::Error::new(name.span(), format!("duplicate `{name}` argument"))
            };
            if name == "fingerprint" {
                let value = if input.peek(Token![=]) {
                    input.parse::<Token![=]>()?;
                    let lit = input.parse::<syn::LitStr>().map_err(|e| {
                        syn::Error::new(
                            e.span(),
                            "`fingerprint` takes a string literal: `fingerprint = \"tag\"`",
                        )
                    })?;
                    Fingerprint::Manual(lit)
                } else {
                    Fingerprint::FromSource
                };
                if fingerprint.replace(value).is_some() {
                    return Err(duplicate(&name));
                }
            } else if name == "in_place" {
                input.parse::<Token![=]>().map_err(|e| {
                    syn::Error::new(
                        e.span(),
                        "`in_place` takes a boolean: `in_place = false` disables \
                         in-place resume arms",
                    )
                })?;
                let lit = input.parse::<syn::LitBool>().map_err(|e| {
                    syn::Error::new(
                        e.span(),
                        "`in_place` takes a boolean: `in_place = false` disables \
                         in-place resume arms",
                    )
                })?;
                if in_place.replace(lit.value).is_some() {
                    return Err(duplicate(&name));
                }
            } else {
                input.parse::<Token![=]>()?;
                let ty: syn::Type = input.parse()?;
                let slot = match name.to_string().as_str() {
                    "yield" => &mut yield_ty,
                    "resume" => &mut resume_ty,
                    _ => {
                        return Err(syn::Error::new(
                            name.span(),
                            "unknown argument, expected `yield`, `resume`, `fingerprint`, \
                             or `in_place`",
                        ));
                    }
                };
                if slot.replace(ty).is_some() {
                    return Err(duplicate(&name));
                }
            }
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        let unit = || syn::parse_quote!(());
        Ok(MacroArgs {
            yield_ty: yield_ty.unwrap_or_else(unit),
            resume_ty: resume_ty.unwrap_or_else(unit),
            fingerprint: fingerprint.unwrap_or(Fingerprint::Off),
            in_place: in_place.unwrap_or(true),
        })
    }
}

#[cfg(test)]
mod tests {
    use quote::quote;
    use syn::parse_quote;

    /// The error message `expand` produces for a coroutine whose
    /// attribute arguments are `attr`.
    fn expand_err(attr: proc_macro2::TokenStream) -> String {
        let item: syn::ItemFn = parse_quote! {
            fn c() {
                yield_!(());
            }
        };
        crate::expand::expand(attr, item).unwrap_err().to_string()
    }

    #[test]
    fn unknown_argument_is_rejected() {
        let msg = expand_err(quote!(banana = i32));
        assert!(
            msg.contains(
                "unknown argument, expected `yield`, `resume`, `fingerprint`, or `in_place`"
            ),
            "got: {msg}"
        );
    }

    #[test]
    fn in_place_takes_a_boolean() {
        let msg = expand_err(quote!(in_place));
        assert!(msg.contains("`in_place` takes a boolean"), "got: {msg}");
        let msg = expand_err(quote!(in_place = 1));
        assert!(msg.contains("`in_place` takes a boolean"), "got: {msg}");
    }

    #[test]
    fn duplicate_in_place_is_rejected() {
        let msg = expand_err(quote!(in_place = false, in_place = true));
        assert!(msg.contains("duplicate `in_place` argument"), "got: {msg}");
    }

    #[test]
    fn in_place_parses() {
        for (attr, expected) in [
            (quote!(yield = u32), true),
            (quote!(yield = u32, in_place = true), true),
            (quote!(yield = u32, in_place = false), false),
        ] {
            let args: super::MacroArgs = syn::parse2(attr).unwrap();
            assert_eq!(args.in_place, expected);
        }
    }

    #[test]
    fn duplicate_type_arguments_are_rejected() {
        let msg = expand_err(quote!(yield = i32, yield = u32));
        assert!(msg.contains("duplicate `yield` argument"), "got: {msg}");
        let msg = expand_err(quote!(resume = i32, resume = u32));
        assert!(msg.contains("duplicate `resume` argument"), "got: {msg}");
    }

    #[test]
    fn non_string_fingerprint_value_is_rejected() {
        let msg = expand_err(quote!(fingerprint = 42));
        assert!(
            msg.contains("`fingerprint` takes a string literal: `fingerprint = \"tag\"`"),
            "got: {msg}"
        );
    }

    #[test]
    fn duplicate_fingerprint_is_rejected() {
        let msg = expand_err(quote!(fingerprint, fingerprint = "tag"));
        assert!(
            msg.contains("duplicate `fingerprint` argument"),
            "got: {msg}"
        );
    }
}
