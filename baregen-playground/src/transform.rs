//! The playground's source-to-source transform.
//!
//! Takes a pasted function with its `#[baregen::coroutine(...)]`
//! attribute still attached, runs the macro pipeline on it, and
//! packages the expansion, the CFG DOT renderings, and any errors
//! (with source positions) into a serializable report.

use baregen_macro_core::expand_debug;

use crate::cfg_dot::cfg_to_dot;
use proc_macro2::{LineColumn, TokenStream};
use serde::Serialize;

/// Everything the playground UI needs to render one transform run.
#[derive(Serialize, Debug)]
pub struct TransformOutput {
    /// The generated code, prettyplease-formatted. Empty on error.
    pub expanded: String,
    /// DOT rendering of the CFG as lowered, before simplification.
    /// `None` when lowering itself failed.
    pub cfg_dot_raw: Option<String>,
    /// DOT rendering of the simplified CFG, annotated with analysis
    /// results when the analysis succeeded.
    pub cfg_dot_simplified: Option<String>,
    /// Empty exactly when the expansion succeeded.
    pub errors: Vec<ErrorInfo>,
}

/// One compile error with its position in the pasted source.
#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct ErrorInfo {
    pub message: String,
    /// 1-based start line.
    pub line: usize,
    /// 0-based start column.
    pub col: usize,
    /// 1-based end line (inclusive).
    pub end_line: usize,
    /// 0-based end column (exclusive).
    pub end_col: usize,
}

/// Runs the whole pipeline on `source`, a function item with the
/// `coroutine` attribute attached (`coroutine` and `baregen::coroutine`
/// path forms are both accepted).
pub fn transform(source: &str) -> TransformOutput {
    let mut item = match syn::parse_str::<syn::ItemFn>(source) {
        Ok(item) => item,
        Err(err) => return TransformOutput::from_error(err),
    };
    let attr_args = match take_coroutine_attr(&mut item) {
        Ok(args) => args,
        Err(err) => return TransformOutput::from_error(err),
    };
    let debug = expand_debug(attr_args, item);
    let cfg_dot_raw = debug
        .cfg_unsimplified
        .as_ref()
        .map(|cfg| cfg_to_dot(cfg, None));
    let cfg_dot_simplified = debug
        .cfg
        .as_ref()
        .map(|cfg| cfg_to_dot(cfg, debug.analysis.as_ref()));
    let (expanded, errors) = match debug.result {
        Ok(tokens) => (pretty(tokens), Vec::new()),
        Err(err) => (String::new(), error_infos(err)),
    };
    TransformOutput {
        expanded,
        cfg_dot_raw,
        cfg_dot_simplified,
        errors,
    }
}

impl TransformOutput {
    fn from_error(err: syn::Error) -> Self {
        TransformOutput {
            expanded: String::new(),
            cfg_dot_raw: None,
            cfg_dot_simplified: None,
            errors: error_infos(err),
        }
    }
}

/// Removes the coroutine attribute from `item` and returns its
/// arguments, exactly as rustc would hand them to the proc macro.
fn take_coroutine_attr(item: &mut syn::ItemFn) -> syn::Result<TokenStream> {
    let pos = item
        .attrs
        .iter()
        .position(is_coroutine_attr)
        .ok_or_else(|| {
            syn::Error::new(
                item.sig.ident.span(),
                "no #[baregen::coroutine(...)] attribute on this function",
            )
        })?;
    let attr = item.attrs.remove(pos);
    match attr.meta {
        syn::Meta::Path(_) => Ok(TokenStream::new()),
        syn::Meta::List(list) => Ok(list.tokens),
        syn::Meta::NameValue(nv) => Err(syn::Error::new_spanned(
            nv,
            "expected #[baregen::coroutine(...)], not a name-value attribute",
        )),
    }
}

fn is_coroutine_attr(attr: &syn::Attribute) -> bool {
    let segments = &attr.path().segments;
    match segments.len() {
        1 => segments[0].ident == "coroutine",
        2 => segments[0].ident == "baregen" && segments[1].ident == "coroutine",
        _ => false,
    }
}

fn pretty(tokens: TokenStream) -> String {
    match syn::parse2::<syn::File>(tokens.clone()) {
        Ok(file) => prettyplease::unparse(&file),
        // The expansion is always a sequence of items; this arm is a
        // safety net so a formatting bug never hides the output.
        Err(_) => tokens.to_string(),
    }
}

fn error_infos(err: syn::Error) -> Vec<ErrorInfo> {
    // `syn::Error` iteration splits an error combined from several
    // messages back into the individual ones, each with its own span.
    err.into_iter()
        .map(|err| {
            let span = err.span();
            let LineColumn { line, column: col } = span.start();
            let LineColumn {
                line: end_line,
                column: end_col,
            } = span.end();
            ErrorInfo {
                message: err.to_string(),
                line,
                col,
                end_line,
                end_col,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_path_attribute_expands() {
        let out = transform(
            "#[baregen::coroutine(yield = u32)]\n\
             fn counter(n: u32) -> u32 {\n\
                 let mut i = 0u32;\n\
                 while i < n {\n\
                     yield_!(i);\n\
                     i += 1;\n\
                 }\n\
                 i\n\
             }",
        );
        assert_eq!(out.errors, vec![]);
        assert!(out.expanded.contains("enum State"), "{}", out.expanded);
        assert!(out.expanded.ends_with('\n'), "prettyplease-formatted");
        let raw = out.cfg_dot_raw.expect("raw CFG dot");
        let simplified = out.cfg_dot_simplified.expect("simplified CFG dot");
        assert!(raw.starts_with("digraph cfg {"));
        assert!(simplified.starts_with("digraph cfg {"));
        // The simplified rendering is annotated with state variant
        // names from the analysis.
        assert!(simplified.contains("S1"), "{simplified}");
    }

    #[test]
    fn bare_path_attribute_expands() {
        let out = transform(
            "#[coroutine]\n\
             fn c() {\n\
                 yield_!(());\n\
             }",
        );
        assert_eq!(out.errors, vec![]);
        assert!(out.expanded.contains("enum State"));
    }

    #[test]
    fn attribute_without_args_expands() {
        let out = transform("#[baregen::coroutine]\nfn c() {\n    yield_!(());\n}");
        assert_eq!(out.errors, vec![]);
    }

    #[test]
    fn other_attributes_are_kept() {
        let out = transform(
            "#[baregen::coroutine(fingerprint)]\n\
             #[derive(Clone)]\n\
             fn c() {\n\
                 yield_!(());\n\
             }",
        );
        assert_eq!(out.errors, vec![]);
        assert!(out.expanded.contains("derive(Clone)"), "{}", out.expanded);
    }

    #[test]
    fn parse_error_is_positioned() {
        let out = transform("fn broken( {\n}");
        assert_eq!(out.expanded, "");
        assert_eq!(out.cfg_dot_raw, None);
        assert_eq!(out.cfg_dot_simplified, None);
        let [err] = &out.errors[..] else {
            panic!("expected one error, got {:?}", out.errors);
        };
        // The lexer error points at the unclosed `(`, line 1 column 9.
        assert_eq!((err.line, err.col), (1, 9), "{err:?}");
    }

    #[test]
    fn missing_attribute_is_an_error() {
        let out = transform("fn c() {\n    yield_!(());\n}");
        let [err] = &out.errors[..] else {
            panic!("expected one error, got {:?}", out.errors);
        };
        assert!(err.message.contains("no #[baregen::coroutine"), "{err:?}");
    }

    #[test]
    fn analyze_error_keeps_cfgs_and_position() {
        // `v` has no syntactic type and crosses a yield: lowering
        // succeeds, the analysis rejects it with a span on `v`.
        let out = transform(
            "#[baregen::coroutine]\n\
             fn c() {\n\
                 let v = compute();\n\
                 yield_!(());\n\
                 drop(v);\n\
             }",
        );
        assert!(out.cfg_dot_raw.is_some());
        assert!(out.cfg_dot_simplified.is_some());
        let [err] = &out.errors[..] else {
            panic!("expected one error, got {:?}", out.errors);
        };
        assert_eq!(
            err.line, 3,
            "error should point at `let v` on line 3: {err:?}"
        );
    }

    #[test]
    fn lower_error_has_no_cfg() {
        let out = transform(
            "#[baregen::coroutine(yield = u32, resume = bool)]\n\
             fn c() {\n\
                 while yield_!(1u32) {\n\
                     f();\n\
                 }\n\
             }",
        );
        assert_eq!(out.cfg_dot_raw, None);
        assert_eq!(out.cfg_dot_simplified, None);
        assert!(!out.errors.is_empty());
        assert_eq!(out.errors[0].line, 3, "{:?}", out.errors);
    }

    #[test]
    fn output_serializes_to_the_documented_shape() {
        let out = transform("#[coroutine]\nfn c() {\n    yield_!(());\n}");
        let json: serde_json::Value = serde_json::to_value(&out).unwrap();
        assert!(json["expanded"].is_string());
        assert!(json["cfg_dot_raw"].is_string());
        assert!(json["cfg_dot_simplified"].is_string());
        assert!(json["errors"].as_array().unwrap().is_empty());

        let err = transform("fn broken( {\n}");
        let json: serde_json::Value = serde_json::to_value(&err).unwrap();
        assert!(json["cfg_dot_raw"].is_null());
        let first = &json["errors"][0];
        for key in ["message", "line", "col", "end_line", "end_col"] {
            assert!(!first[key].is_null(), "missing key {key}");
        }
    }
}
