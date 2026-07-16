use syn::Token;
use syn::ext::IdentExt;
use syn::parse::{Parse, ParseStream};

/// Arguments of `#[baregen::coroutine(yield = Type, resume = Type,
/// fingerprint)]`. The types default to `()`.
pub struct MacroArgs {
    pub yield_ty: syn::Type,
    pub resume_ty: syn::Type,
    /// Whether the `fingerprint` flag was given (injects the `__fp`
    /// field and its checks).
    pub fingerprint: bool,
}

impl Parse for MacroArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut yield_ty = None;
        let mut resume_ty = None;
        let mut fingerprint = false;
        while !input.is_empty() {
            // `yield` is a reserved keyword, so accept any identifier here.
            let name = input.call(syn::Ident::parse_any)?;
            let duplicate =
                |name: &syn::Ident| syn::Error::new(name.span(), format!("duplicate `{name}` argument"));
            if name == "fingerprint" {
                if std::mem::replace(&mut fingerprint, true) {
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
                            "unknown argument, expected `yield`, `resume`, or `fingerprint`",
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
            fingerprint,
        })
    }
}
