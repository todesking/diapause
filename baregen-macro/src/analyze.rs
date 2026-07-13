//! Liveness analysis: decides which variables must be stored in each
//! intermediate state of the generated state machine, and which borrows
//! must be reconstructed after a resume.

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

/// A direct borrow (`let y = &x;` / `let y = &mut x;`) that crossed a
/// yield and must be re-established at the head of a later segment.
#[derive(Debug)]
pub struct Reborrow {
    pub target: syn::Ident,
    pub target_mut: Option<syn::Token![mut]>,
    pub source: syn::Ident,
    pub mutable: bool,
}

#[derive(Debug)]
pub struct Analysis {
    /// Fields of each intermediate state `S1..Sn` (one entry per yield).
    pub states: Vec<Vec<StateField>>,
    /// `reborrows[k]`: borrow statements to prepend to segment k's arm.
    pub reborrows: Vec<Vec<Reborrow>>,
    /// `removed_stmts[k]`: indices of statements in segment k to omit
    /// (original borrow `let`s whose binding is only used after a yield).
    pub removed_stmts: Vec<HashSet<usize>>,
}

#[derive(Debug, Clone)]
enum BorrowKind {
    NotABorrow,
    /// `let y = &x;` / `let y = &mut x;` with a plain identifier source.
    Direct {
        source: syn::Ident,
        mutable: bool,
    },
    /// A reference that cannot be reconstructed; the message explains why.
    NonReconstructible {
        why: &'static str,
    },
}

#[derive(Debug)]
struct DefRecord {
    var: VarDef,
    borrow: BorrowKind,
    segment: usize,
    /// Index within the segment's statements; `None` for arguments and
    /// resume bindings.
    stmt_idx: Option<usize>,
}

/// Analyzes the coroutine body.
///
/// State `S{j+1}` holds the variables that are live across `yields[j]`,
/// i.e. defined in segments `0..=j` and used in segments `j+1..`. A
/// variable bound by a direct borrow is never stored: its source is
/// stored instead and the borrow is re-created after resume.
pub fn analyze(args: &[VarDef], ir: &CoroutineIr, resume_ty: &syn::Type) -> syn::Result<Analysis> {
    let n = ir.yields.len();
    let mut errors: Option<syn::Error> = None;
    let push_error = |errors: &mut Option<syn::Error>, e: syn::Error| match errors {
        Some(prev) => prev.combine(e),
        None => *errors = Some(e),
    };

    // === Definitions, in program order ===

    // env: name -> known type at the current point in program order.
    // Used for move propagation (`let y = x;` copies x's type).
    let mut env: HashMap<String, syn::Type> = args
        .iter()
        .filter_map(|a| a.ty.clone().map(|ty| (a.ident.to_string(), ty)))
        .collect();

    let mut defs: Vec<DefRecord> = args
        .iter()
        .map(|a| DefRecord {
            var: a.clone(),
            borrow: BorrowKind::NotABorrow,
            segment: 0,
            stmt_idx: None,
        })
        .collect();
    for (k, seg) in ir.segments.iter().enumerate() {
        if k > 0
            && let Some(rb) = &ir.yields[k - 1].resume_binding
        {
            let ty = rb.ty.clone().unwrap_or_else(|| resume_ty.clone());
            env.insert(rb.ident.to_string(), ty.clone());
            defs.push(DefRecord {
                var: VarDef {
                    ident: rb.ident.clone(),
                    mutability: rb.mutability,
                    ty: Some(ty),
                },
                borrow: BorrowKind::NotABorrow,
                segment: k,
                stmt_idx: None,
            });
        }
        for (i, stmt) in seg.stmts.iter().enumerate() {
            let before = defs.len();
            collect_let_defs(stmt, k, i, &mut defs, &env);
            for d in &defs[before..] {
                match &d.var.ty {
                    Some(ty) => env.insert(d.var.ident.to_string(), ty.clone()),
                    // A def of unknown type shadows any earlier known one.
                    None => env.remove(&d.var.ident.to_string()),
                };
            }
        }
    }

    // === Uses per segment ===

    // uses[k]: identifiers appearing in segment k, including the value
    // expression of the yield that terminates it (evaluated in the same
    // match arm, before the state transition).
    let mut uses: Vec<HashSet<String>> = ir
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

    // A reborrowed target needs its source in the same segment, so a use
    // of the target implies a use of the source. Iterate to a fixpoint to
    // handle chains (`let y = &x; let z = &y;`).
    let direct_borrows: Vec<(String, String, usize)> = defs
        .iter()
        .filter_map(|d| match &d.borrow {
            BorrowKind::Direct { source, .. } => {
                Some((d.var.ident.to_string(), source.to_string(), d.segment))
            }
            _ => None,
        })
        .collect();
    loop {
        let mut changed = false;
        for (target, source, def_seg) in &direct_borrows {
            for seg_uses in &mut uses[def_seg + 1..] {
                if seg_uses.contains(target) && seg_uses.insert(source.clone()) {
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    let crosses = |d: &DefRecord| {
        let name = d.var.ident.to_string();
        (d.segment + 1..=n).any(|k| uses[k].contains(&name))
    };

    // === Errors for borrows that cannot be reconstructed ===

    for d in &defs {
        if let BorrowKind::NonReconstructible { why } = &d.borrow
            && crosses(d)
        {
            let name = &d.var.ident;
            push_error(
                &mut errors,
                syn::Error::new(d.var.ident.span(), format!("`{name}` {why}")),
            );
        }
    }

    // Sources reborrowed mutably must be bound `mut` when unpacked.
    let mutable_sources: HashSet<String> = defs
        .iter()
        .filter(|d| crosses(d))
        .filter_map(|d| match &d.borrow {
            BorrowKind::Direct {
                source,
                mutable: true,
            } => Some(source.to_string()),
            _ => None,
        })
        .collect();

    // === State fields ===

    let mut states = Vec::with_capacity(n);
    for j in 0..n {
        let used_after: HashSet<&String> = uses[j + 1..].iter().flatten().collect();

        // The last definition of a name shadows the earlier ones.
        let mut last_def: HashMap<String, &DefRecord> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for d in defs.iter().filter(|d| d.segment <= j) {
            let name = d.var.ident.to_string();
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
            match &d.borrow {
                // Reconstructed after resume, never stored. Errors for
                // NonReconstructible were already reported above.
                BorrowKind::Direct { .. } | BorrowKind::NonReconstructible { .. } => continue,
                BorrowKind::NotABorrow => {}
            }
            match &d.var.ty {
                Some(ty) => {
                    let forced_mut = mutable_sources
                        .contains(name)
                        .then(|| syn::Token![mut](d.var.ident.span()));
                    fields.push(StateField {
                        ident: d.var.ident.clone(),
                        mutability: d.var.mutability.or(forced_mut),
                        ty: ty.clone(),
                    });
                }
                None => push_error(
                    &mut errors,
                    syn::Error::new(
                        d.var.ident.span(),
                        format!(
                            "cannot determine the type of `{name}`, which is held across \
                             yield_!; write an explicit type annotation: `let {name}: Type = ...`"
                        ),
                    ),
                ),
            }
        }
        states.push(fields);
    }

    // === Reborrow statements and original-borrow removal ===

    let mut reborrows: Vec<Vec<Reborrow>> = (0..=n).map(|_| Vec::new()).collect();
    for (k, rb) in reborrows.iter_mut().enumerate().skip(1) {
        for d in &defs {
            let BorrowKind::Direct { source, mutable } = &d.borrow else {
                continue;
            };
            if d.segment < k && uses[k].contains(&d.var.ident.to_string()) {
                rb.push(Reborrow {
                    target: d.var.ident.clone(),
                    target_mut: d.var.mutability,
                    source: source.clone(),
                    mutable: *mutable,
                });
            }
        }
    }

    let mut removed_stmts: Vec<HashSet<usize>> = vec![HashSet::new(); n + 1];
    for d in &defs {
        let (BorrowKind::Direct { .. }, Some(i)) = (&d.borrow, d.stmt_idx) else {
            continue;
        };
        if !crosses(d) {
            continue;
        }
        // Drop the original `let` unless the binding is still used later
        // within its own segment (borrows have no side effects).
        let mut c = UseCollector::default();
        for stmt in &ir.segments[d.segment].stmts[i + 1..] {
            c.visit_stmt(stmt);
        }
        if d.segment < n {
            c.visit_expr(&ir.yields[d.segment].value);
        }
        if !c.used.contains(&d.var.ident.to_string()) {
            removed_stmts[d.segment].insert(i);
        }
    }

    match errors {
        Some(e) => Err(e),
        None => Ok(Analysis {
            states,
            reborrows,
            removed_stmts,
        }),
    }
}

/// Records the variables bound by a `let` statement, determining their
/// types where possible (explicit annotation, literal suffix, or move
/// propagation) and classifying direct borrows.
fn collect_let_defs(
    stmt: &syn::Stmt,
    segment: usize,
    stmt_idx: usize,
    out: &mut Vec<DefRecord>,
    env: &HashMap<String, syn::Type>,
) {
    let syn::Stmt::Local(local) = stmt else {
        return;
    };
    let init = local.init.as_ref().map(|init| &*init.expr);
    let mut push = |var: VarDef, borrow: BorrowKind| {
        out.push(DefRecord {
            var,
            borrow,
            segment,
            stmt_idx: Some(stmt_idx),
        })
    };
    match &local.pat {
        syn::Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => push(
            VarDef {
                ident: pi.ident.clone(),
                mutability: pi.mutability,
                ty: init.and_then(|e| infer_expr_ty(e, env)),
            },
            classify_borrow(init, None),
        ),
        syn::Pat::Type(pt) => match &*pt.pat {
            syn::Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => push(
                VarDef {
                    ident: pi.ident.clone(),
                    mutability: pi.mutability,
                    ty: Some((*pt.ty).clone()),
                },
                classify_borrow(init, Some(&pt.ty)),
            ),
            other => collect_pat_idents(other, segment, stmt_idx, out),
        },
        other => collect_pat_idents(other, segment, stmt_idx, out),
    }
}

fn classify_borrow(init: Option<&syn::Expr>, annotated: Option<&syn::Type>) -> BorrowKind {
    let mut init = init;
    while let Some(syn::Expr::Paren(p)) = init {
        init = Some(&p.expr);
    }
    match init {
        Some(syn::Expr::Reference(r)) => match &*r.expr {
            syn::Expr::Path(p) if p.qself.is_none() && p.path.get_ident().is_some() => {
                BorrowKind::Direct {
                    source: p.path.get_ident().unwrap().clone(),
                    mutable: r.mutability.is_some(),
                }
            }
            _ => BorrowKind::NonReconstructible {
                why: "is held across yield_! but borrows a non-trivial place; only direct \
                      borrows of local variables (`let y = &x;` / `let y = &mut x;`) can be \
                      reconstructed after resume",
            },
        },
        _ if matches!(annotated, Some(syn::Type::Reference(_))) => BorrowKind::NonReconstructible {
            why: "has a reference type but is not a direct borrow (`let y = &x;` / \
                      `let y = &mut x;`), so it cannot be held across yield_!",
        },
        _ => BorrowKind::NotABorrow,
    }
}

/// Fallback for complex patterns: every bound identifier becomes a def of
/// unknown type.
fn collect_pat_idents(pat: &syn::Pat, segment: usize, stmt_idx: usize, out: &mut Vec<DefRecord>) {
    struct Collector<'a> {
        out: &'a mut Vec<DefRecord>,
        segment: usize,
        stmt_idx: usize,
    }
    impl<'ast> Visit<'ast> for Collector<'_> {
        fn visit_pat_ident(&mut self, pi: &'ast syn::PatIdent) {
            self.out.push(DefRecord {
                var: VarDef {
                    ident: pi.ident.clone(),
                    mutability: pi.mutability,
                    ty: None,
                },
                borrow: BorrowKind::NotABorrow,
                segment: self.segment,
                stmt_idx: Some(self.stmt_idx),
            });
            syn::visit::visit_pat_ident(self, pi);
        }
    }
    Collector {
        out,
        segment,
        stmt_idx,
    }
    .visit_pat(pat);
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

    fn states(block: &syn::Block, args: &[VarDef], resume_ty: &syn::Type) -> Vec<Vec<StateField>> {
        let ir = parse_body(block).unwrap();
        analyze(args, &ir, resume_ty).unwrap().states
    }

    fn field_names(fields: &[StateField]) -> Vec<String> {
        fields.iter().map(|f| f.ident.to_string()).collect()
    }

    fn error_of(block: &syn::Block) -> syn::Error {
        let ir = parse_body(block).unwrap();
        analyze(&[], &ir, &unit()).unwrap_err()
    }

    #[test]
    fn unused_vars_are_not_stored() {
        let block: syn::Block = parse_quote!({
            let a: i32 = 1;
            let b: i32 = 2;
            yield_!(1);
            a
        });
        let states = states(&block, &[], &unit());
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
        let arg = VarDef {
            ident: parse_quote!(x),
            mutability: None,
            ty: Some(parse_quote!(u32)),
        };
        let states = states(&block, &[arg], &unit());
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
        let states = states(&block, &[], &unit());
        assert!(states[0].is_empty());
    }

    #[test]
    fn resume_binding_defaults_to_resume_type() {
        let block: syn::Block = parse_quote!({
            let r = yield_!(1);
            yield_!(2);
            r
        });
        let resume_ty: syn::Type = parse_quote!(String);
        let states = states(&block, &[], &resume_ty);
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
        let states = states(&block, &[], &unit());
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
        let states = states(&block, &[], &unit());
        assert_eq!(field_names(&states[0]), ["a", "b", "c", "d"]);
        let tys: Vec<syn::Type> = vec![
            parse_quote!(u8),
            parse_quote!(f32),
            parse_quote!(bool),
            parse_quote!(char),
        ];
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
        assert!(error_of(&block).to_string().contains("type annotation"));
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
        let states = states(&block, &[], &unit());
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
        let arg = VarDef {
            ident: parse_quote!(x),
            mutability: None,
            ty: Some(parse_quote!(u32)),
        };
        let states = states(&block, &[arg], &unit());
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
        let resume_ty: syn::Type = parse_quote!(String);
        let states = states(&block, &[], &resume_ty);
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
        assert!(error_of(&block).to_string().contains("type annotation"));
    }

    #[test]
    fn unknown_type_is_an_error() {
        let block: syn::Block = parse_quote!({
            let x = compute();
            yield_!(1);
            x
        });
        assert!(error_of(&block).to_string().contains("type annotation"));
    }

    #[test]
    fn macro_tokens_count_as_uses() {
        let block: syn::Block = parse_quote!({
            let a: i32 = 1;
            yield_!(1);
            println!("{}", a);
        });
        let states = states(&block, &[], &unit());
        assert_eq!(field_names(&states[0]), ["a"]);
    }

    #[test]
    fn borrow_target_is_replaced_by_its_source() {
        let block: syn::Block = parse_quote!({
            let mut x: i32 = 1;
            let y = &mut x;
            yield_!(1);
            *y += 1;
        });
        let ir = parse_body(&block).unwrap();
        let a = analyze(&[], &ir, &unit()).unwrap();
        // y is reconstructed, x is stored (bound mutably for the reborrow)
        assert_eq!(field_names(&a.states[0]), ["x"]);
        assert!(a.states[0][0].mutability.is_some());
        assert_eq!(a.reborrows[1].len(), 1);
        assert_eq!(a.reborrows[1][0].target, "y");
        assert_eq!(a.reborrows[1][0].source, "x");
        assert!(a.reborrows[1][0].mutable);
        // the original `let y = &mut x;` (stmt 1 of segment 0) is dropped
        assert!(a.removed_stmts[0].contains(&1));
    }

    #[test]
    fn borrow_used_before_yield_keeps_original_stmt() {
        let block: syn::Block = parse_quote!({
            let mut x: i32 = 1;
            let y = &mut x;
            *y += 1;
            yield_!(1);
            *y += 1;
        });
        let ir = parse_body(&block).unwrap();
        let a = analyze(&[], &ir, &unit()).unwrap();
        assert!(a.removed_stmts[0].is_empty());
        assert_eq!(a.reborrows[1].len(), 1);
    }

    #[test]
    fn shared_borrow_does_not_force_mut() {
        let block: syn::Block = parse_quote!({
            let x: String = mk();
            let y = &x;
            yield_!(1);
            y.len()
        });
        let ir = parse_body(&block).unwrap();
        let a = analyze(&[], &ir, &unit()).unwrap();
        assert_eq!(field_names(&a.states[0]), ["x"]);
        assert!(a.states[0][0].mutability.is_none());
        assert!(!a.reborrows[1][0].mutable);
    }

    #[test]
    fn borrow_chain_reborrows_in_definition_order() {
        let block: syn::Block = parse_quote!({
            let x: i32 = 1;
            let y = &x;
            let z = &y;
            yield_!(1);
            f(z);
        });
        let ir = parse_body(&block).unwrap();
        let a = analyze(&[], &ir, &unit()).unwrap();
        assert_eq!(field_names(&a.states[0]), ["x"]);
        let order: Vec<_> = a.reborrows[1]
            .iter()
            .map(|r| r.target.to_string())
            .collect();
        assert_eq!(order, ["y", "z"]);
    }

    #[test]
    fn complex_borrow_across_yield_is_an_error() {
        let block: syn::Block = parse_quote!({
            let y = &x.field;
            yield_!(1);
            f(y);
        });
        assert!(error_of(&block).to_string().contains("non-trivial place"));
    }

    #[test]
    fn reference_typed_non_borrow_across_yield_is_an_error() {
        let block: syn::Block = parse_quote!({
            let y: &u32 = first(v);
            yield_!(1);
            f(y);
        });
        assert!(error_of(&block).to_string().contains("reference type"));
    }

    #[test]
    fn non_crossing_borrows_are_untouched() {
        let block: syn::Block = parse_quote!({
            let y = &x.field;
            f(y);
            yield_!(1);
        });
        let ir = parse_body(&block).unwrap();
        let a = analyze(&[], &ir, &unit()).unwrap();
        assert!(a.states[0].is_empty());
        assert!(a.reborrows[1].is_empty());
        assert!(a.removed_stmts[0].is_empty());
    }
}
