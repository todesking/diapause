//! Liveness analysis: decides which variables must be stored in each
//! intermediate state of the generated state machine.

use std::collections::{HashMap, HashSet};

use syn::visit::Visit;

use crate::parse::CoroutineIr;

/// A variable binding introduced in the coroutine body (argument, `let`,
/// or resume binding). `ty` is `None` when the type could not be
/// determined syntactically.
#[derive(Debug, Clone)]
pub struct VarDef {
    pub ident: syn::Ident,
    pub mutability: Option<syn::Token![mut]>,
    pub ty: Option<syn::Type>,
}

/// A field of an intermediate state variant.
#[derive(Debug)]
pub struct StateField {
    pub ident: syn::Ident,
    pub mutability: Option<syn::Token![mut]>,
    pub ty: syn::Type,
}

/// Computes the fields of each intermediate state `S1..Sn`.
///
/// The returned vector has one entry per yield point: entry `j` holds the
/// variables that are live across `yields[j]`, i.e. defined in segments
/// `0..=j` and used in segments `j+1..`.
pub fn live_states(
    args: &[VarDef],
    ir: &CoroutineIr,
    resume_ty: &syn::Type,
) -> syn::Result<Vec<Vec<StateField>>> {
    let n = ir.yields.len();

    // env: name -> known type at the current point in program order.
    // Used for move propagation (`let y = x;` copies x's type).
    let mut env: HashMap<String, syn::Type> = args
        .iter()
        .filter_map(|a| a.ty.clone().map(|ty| (a.ident.to_string(), ty)))
        .collect();

    let mut defs: Vec<Vec<VarDef>> = vec![Vec::new(); n + 1];
    defs[0].extend(args.iter().cloned());
    for (k, seg) in ir.segments.iter().enumerate() {
        if k > 0
            && let Some(rb) = &ir.yields[k - 1].resume_binding
        {
            let ty = rb.ty.clone().unwrap_or_else(|| resume_ty.clone());
            env.insert(rb.ident.to_string(), ty.clone());
            defs[k].push(VarDef {
                ident: rb.ident.clone(),
                mutability: rb.mutability,
                ty: Some(ty),
            });
        }
        for stmt in &seg.stmts {
            let before = defs[k].len();
            collect_let_defs(stmt, &mut defs[k], &env);
            for d in &defs[k][before..] {
                match &d.ty {
                    Some(ty) => env.insert(d.ident.to_string(), ty.clone()),
                    // A def of unknown type shadows any earlier known one.
                    None => env.remove(&d.ident.to_string()),
                };
            }
        }
    }

    // uses[k]: identifiers appearing in segment k, including the value
    // expression of the yield that terminates it (evaluated in the same
    // match arm, before the state transition).
    let uses: Vec<HashSet<String>> = ir
        .segments
        .iter()
        .enumerate()
        .map(|(k, seg)| {
            let mut c = UseCollector::default();
            for stmt in &seg.stmts {
                c.visit_stmt(stmt);
            }
            if k < n {
                c.visit_expr(&ir.yields[k].value);
            }
            c.used
        })
        .collect();

    let mut errors: Option<syn::Error> = None;
    let mut states = Vec::with_capacity(n);
    for j in 0..n {
        let used_after: HashSet<&String> = uses[j + 1..].iter().flatten().collect();

        // The last definition of a name shadows the earlier ones.
        let mut last_def: HashMap<String, &VarDef> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for d in defs[..=j].iter().flatten() {
            let name = d.ident.to_string();
            if last_def.insert(name.clone(), d).is_some() {
                order.retain(|n| n != &name);
            }
            order.push(name);
        }

        let mut fields = Vec::new();
        for name in &order {
            if !used_after.contains(name) {
                continue;
            }
            let d = last_def[name];
            match &d.ty {
                Some(ty) => fields.push(StateField {
                    ident: d.ident.clone(),
                    mutability: d.mutability,
                    ty: ty.clone(),
                }),
                None => {
                    let e = syn::Error::new(
                        d.ident.span(),
                        format!(
                            "cannot determine the type of `{name}`, which is held across \
                             yield_!; write an explicit type annotation: `let {name}: Type = ...`"
                        ),
                    );
                    match &mut errors {
                        Some(prev) => prev.combine(e),
                        None => errors = Some(e),
                    }
                }
            }
        }
        states.push(fields);
    }

    match errors {
        Some(e) => Err(e),
        None => Ok(states),
    }
}

/// Records the variables bound by a `let` statement, determining their
/// types where possible: explicit annotation, literal suffix, or move
/// propagation from a variable of known type.
fn collect_let_defs(stmt: &syn::Stmt, out: &mut Vec<VarDef>, env: &HashMap<String, syn::Type>) {
    let syn::Stmt::Local(local) = stmt else {
        return;
    };
    let init_ty = || {
        local
            .init
            .as_ref()
            .and_then(|init| infer_expr_ty(&init.expr, env))
    };
    match &local.pat {
        syn::Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => out.push(VarDef {
            ident: pi.ident.clone(),
            mutability: pi.mutability,
            ty: init_ty(),
        }),
        syn::Pat::Type(pt) => match &*pt.pat {
            syn::Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => {
                out.push(VarDef {
                    ident: pi.ident.clone(),
                    mutability: pi.mutability,
                    ty: Some((*pt.ty).clone()),
                })
            }
            other => collect_pat_idents(other, out),
        },
        other => collect_pat_idents(other, out),
    }
}

/// Syntactic type inference for an initializer expression.
fn infer_expr_ty(expr: &syn::Expr, env: &HashMap<String, syn::Type>) -> Option<syn::Type> {
    match expr {
        syn::Expr::Paren(p) => infer_expr_ty(&p.expr, env),
        // Negation of a suffixed numeric literal keeps the literal's type.
        syn::Expr::Unary(u) if matches!(u.op, syn::UnOp::Neg(_)) => match &*u.expr {
            syn::Expr::Lit(_) => infer_expr_ty(&u.expr, env),
            _ => None,
        },
        syn::Expr::Lit(lit) => infer_lit_ty(&lit.lit),
        // Move propagation: `let y = x;` where x's type is known.
        syn::Expr::Path(p) if p.qself.is_none() => {
            let ident = p.path.get_ident()?;
            env.get(&ident.to_string()).cloned()
        }
        _ => None,
    }
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

/// Fallback for complex patterns: every bound identifier becomes a def of
/// unknown type.
fn collect_pat_idents(pat: &syn::Pat, out: &mut Vec<VarDef>) {
    struct Collector<'a>(&'a mut Vec<VarDef>);
    impl<'ast> Visit<'ast> for Collector<'_> {
        fn visit_pat_ident(&mut self, pi: &'ast syn::PatIdent) {
            self.0.push(VarDef {
                ident: pi.ident.clone(),
                mutability: pi.mutability,
                ty: None,
            });
            syn::visit::visit_pat_ident(self, pi);
        }
    }
    Collector(out).visit_pat(pat);
}

/// Collects identifiers that may refer to local variables.
///
/// Overapproximates: every unqualified single-segment path counts, and all
/// identifiers inside macro invocations are taken verbatim from the token
/// stream. A false positive only keeps a variable alive longer than
/// necessary.
#[derive(Default)]
struct UseCollector {
    used: HashSet<String>,
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

fn collect_token_idents(tokens: proc_macro2::TokenStream, out: &mut HashSet<String>) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_body;
    use syn::parse_quote;

    fn unit() -> syn::Type {
        parse_quote!(())
    }

    fn field_names(fields: &[StateField]) -> Vec<String> {
        fields.iter().map(|f| f.ident.to_string()).collect()
    }

    #[test]
    fn unused_vars_are_not_stored() {
        let block: syn::Block = parse_quote!({
            let a: i32 = 1;
            let b: i32 = 2;
            yield_!(1);
            a
        });
        let ir = parse_body(&block).unwrap();
        let states = live_states(&[], &ir, &unit()).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(field_names(&states[0]), ["a"]);
    }

    #[test]
    fn args_live_across_yields() {
        let block: syn::Block = parse_quote!({
            yield_!(1);
            yield_!(2);
            x
        });
        let ir = parse_body(&block).unwrap();
        let arg = VarDef {
            ident: parse_quote!(x),
            mutability: None,
            ty: Some(parse_quote!(u32)),
        };
        let states = live_states(&[arg], &ir, &unit()).unwrap();
        assert_eq!(field_names(&states[0]), ["x"]);
        assert_eq!(field_names(&states[1]), ["x"]);
    }

    #[test]
    fn yield_value_use_does_not_keep_var_alive() {
        // `a` is consumed by the first yield's value expression, which is
        // evaluated before the transition, so S1 must not store it.
        let block: syn::Block = parse_quote!({
            let a: i32 = 1;
            yield_!(a);
        });
        let ir = parse_body(&block).unwrap();
        let states = live_states(&[], &ir, &unit()).unwrap();
        assert!(states[0].is_empty());
    }

    #[test]
    fn resume_binding_defaults_to_resume_type() {
        let block: syn::Block = parse_quote!({
            let r = yield_!(1);
            yield_!(2);
            r
        });
        let ir = parse_body(&block).unwrap();
        let resume_ty: syn::Type = parse_quote!(String);
        let states = live_states(&[], &ir, &resume_ty).unwrap();
        assert!(states[0].is_empty());
        assert_eq!(field_names(&states[1]), ["r"]);
        assert_eq!(states[1][0].ty, resume_ty);
    }

    #[test]
    fn shadowing_last_def_wins() {
        let block: syn::Block = parse_quote!({
            let x: i32 = 1;
            let x: String = format!("{x}");
            yield_!(1);
            x
        });
        let ir = parse_body(&block).unwrap();
        let states = live_states(&[], &ir, &unit()).unwrap();
        assert_eq!(field_names(&states[0]), ["x"]);
        let expected: syn::Type = parse_quote!(String);
        assert_eq!(states[0][0].ty, expected);
    }

    #[test]
    fn literal_suffix_determines_type() {
        let block: syn::Block = parse_quote!({
            let a = 123u8;
            let b = -1.5f32;
            let c = true;
            let d = 'x';
            yield_!(1);
            f(a, b, c, d);
        });
        let ir = parse_body(&block).unwrap();
        let states = live_states(&[], &ir, &unit()).unwrap();
        assert_eq!(field_names(&states[0]), ["a", "b", "c", "d"]);
        let tys: Vec<syn::Type> =
            vec![parse_quote!(u8), parse_quote!(f32), parse_quote!(bool), parse_quote!(char)];
        for (f, ty) in states[0].iter().zip(&tys) {
            assert_eq!(&f.ty, ty);
        }
    }

    #[test]
    fn unsuffixed_literal_is_not_inferred() {
        let block: syn::Block = parse_quote!({
            let a = 123;
            yield_!(1);
            f(a);
        });
        let ir = parse_body(&block).unwrap();
        assert!(live_states(&[], &ir, &unit()).is_err());
    }

    #[test]
    fn move_propagates_types() {
        let block: syn::Block = parse_quote!({
            let a: String = mk();
            let b = a;
            let c = b;
            yield_!(1);
            c
        });
        let ir = parse_body(&block).unwrap();
        let states = live_states(&[], &ir, &unit()).unwrap();
        assert_eq!(field_names(&states[0]), ["c"]);
        let expected: syn::Type = parse_quote!(String);
        assert_eq!(states[0][0].ty, expected);
    }

    #[test]
    fn move_propagates_from_argument() {
        let block: syn::Block = parse_quote!({
            let y = x;
            yield_!(1);
            y
        });
        let ir = parse_body(&block).unwrap();
        let arg = VarDef {
            ident: parse_quote!(x),
            mutability: None,
            ty: Some(parse_quote!(u32)),
        };
        let states = live_states(&[arg], &ir, &unit()).unwrap();
        let expected: syn::Type = parse_quote!(u32);
        assert_eq!(states[0][0].ty, expected);
    }

    #[test]
    fn move_propagates_across_yields() {
        let block: syn::Block = parse_quote!({
            let r = yield_!(1);
            let s = r;
            yield_!(2);
            s
        });
        let ir = parse_body(&block).unwrap();
        let resume_ty: syn::Type = parse_quote!(String);
        let states = live_states(&[], &ir, &resume_ty).unwrap();
        assert_eq!(field_names(&states[1]), ["s"]);
        assert_eq!(states[1][0].ty, resume_ty);
    }

    #[test]
    fn shadowing_by_unknown_type_stops_propagation() {
        let block: syn::Block = parse_quote!({
            let a: u32 = 1;
            let a = mk();
            let b = a;
            yield_!(1);
            b
        });
        let ir = parse_body(&block).unwrap();
        assert!(live_states(&[], &ir, &unit()).is_err());
    }

    #[test]
    fn unknown_type_is_an_error() {
        let block: syn::Block = parse_quote!({
            let x = compute();
            yield_!(1);
            x
        });
        let ir = parse_body(&block).unwrap();
        let err = live_states(&[], &ir, &unit()).unwrap_err();
        assert!(err.to_string().contains("type annotation"));
    }

    #[test]
    fn macro_tokens_count_as_uses() {
        let block: syn::Block = parse_quote!({
            let a: i32 = 1;
            yield_!(1);
            println!("{}", a);
        });
        let ir = parse_body(&block).unwrap();
        let states = live_states(&[], &ir, &unit()).unwrap();
        assert_eq!(field_names(&states[0]), ["a"]);
    }
}
