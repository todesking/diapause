//! Parses a coroutine body into segments separated by `yield_!` points.

use syn::spanned::Spanned;
use syn::visit::Visit;

/// The coroutine body split at its suspension points.
///
/// For `n` yields there are `n + 1` segments; `yields[k]` sits between
/// `segments[k]` and `segments[k + 1]`.
#[derive(Debug)]
pub struct CoroutineIr {
    pub segments: Vec<Segment>,
    pub yields: Vec<YieldPoint>,
}

#[derive(Debug, Default)]
pub struct Segment {
    pub stmts: Vec<syn::Stmt>,
}

#[derive(Debug)]
pub struct YieldPoint {
    /// The expression inside `yield_!(...)`.
    pub value: syn::Expr,
    /// The binding in `let r = yield_!(...);`, if any.
    pub resume_binding: Option<ResumeBinding>,
}

#[derive(Debug)]
pub struct ResumeBinding {
    pub ident: syn::Ident,
    pub mutability: Option<syn::Token![mut]>,
    pub ty: Option<syn::Type>,
}

pub fn parse_body(block: &syn::Block) -> syn::Result<CoroutineIr> {
    let mut segments = vec![Segment::default()];
    let mut yields = Vec::new();
    let mut errors: Option<syn::Error> = None;
    let mut push_error = |e: syn::Error| match &mut errors {
        Some(prev) => prev.combine(e),
        None => errors = Some(e),
    };

    for stmt in &block.stmts {
        match classify(stmt) {
            Ok(Some(y)) => {
                yields.push(y);
                segments.push(Segment::default());
            }
            Ok(None) => {
                if let Err(e) = validate_no_yield(stmt) {
                    push_error(e);
                }
                segments.last_mut().unwrap().stmts.push(stmt.clone());
            }
            Err(e) => push_error(e),
        }
    }

    match errors {
        Some(e) => Err(e),
        None => Ok(CoroutineIr { segments, yields }),
    }
}

/// Returns `Some(YieldPoint)` if the statement is one of the two allowed
/// yield forms, `None` for an ordinary statement.
fn classify(stmt: &syn::Stmt) -> syn::Result<Option<YieldPoint>> {
    match stmt {
        // `yield_!(expr);`
        syn::Stmt::Macro(stmt_macro) if is_yield_macro(&stmt_macro.mac) => {
            if stmt_macro.semi_token.is_none() {
                return Err(syn::Error::new_spanned(
                    stmt_macro,
                    "yield_! as the trailing expression is not supported; add a semicolon",
                ));
            }
            Ok(Some(YieldPoint {
                value: parse_yield_value(&stmt_macro.mac)?,
                resume_binding: None,
            }))
        }
        // `let r = yield_!(expr);`
        syn::Stmt::Local(local)
            if matches!(
                local.init.as_ref().map(|i| &*i.expr),
                Some(syn::Expr::Macro(m)) if is_yield_macro(&m.mac)
            ) =>
        {
            let init = local.init.as_ref().unwrap();
            if let Some((else_token, _)) = &init.diverge {
                return Err(syn::Error::new_spanned(
                    else_token,
                    "`let ... else` cannot be used with yield_!",
                ));
            }
            let syn::Expr::Macro(m) = &*init.expr else {
                unreachable!()
            };
            let binding = parse_resume_binding(&local.pat)?;
            Ok(Some(YieldPoint {
                value: parse_yield_value(&m.mac)?,
                resume_binding: Some(binding),
            }))
        }
        _ => Ok(None),
    }
}

fn parse_yield_value(mac: &syn::Macro) -> syn::Result<syn::Expr> {
    let value: syn::Expr = if mac.tokens.is_empty() {
        syn::parse_quote!(())
    } else {
        mac.parse_body().map_err(|_| {
            syn::Error::new_spanned(&mac.tokens, "yield_! takes a single expression")
        })?
    };
    // The yielded expression itself must not suspend.
    validate_expr_no_yield(&value)?;
    Ok(value)
}

fn parse_resume_binding(pat: &syn::Pat) -> syn::Result<ResumeBinding> {
    let (pat, ty) = match pat {
        syn::Pat::Type(pt) => (&*pt.pat, Some((*pt.ty).clone())),
        other => (other, None),
    };
    match pat {
        syn::Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => Ok(ResumeBinding {
            ident: pi.ident.clone(),
            mutability: pi.mutability,
            ty,
        }),
        other => Err(syn::Error::new_spanned(
            other,
            "the binding of `let ... = yield_!(...)` must be a simple identifier",
        )),
    }
}

fn is_yield_macro(mac: &syn::Macro) -> bool {
    mac.path
        .segments
        .last()
        .is_some_and(|seg| seg.ident == "yield_")
}

/// Rejects any `yield_!` occurrence inside an ordinary statement.
fn validate_no_yield(stmt: &syn::Stmt) -> syn::Result<()> {
    let mut visitor = YieldFinder::default();
    visitor.visit_stmt(stmt);
    match visitor.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn validate_expr_no_yield(expr: &syn::Expr) -> syn::Result<()> {
    let mut visitor = YieldFinder::default();
    visitor.visit_expr(expr);
    match visitor.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[derive(Debug, Default)]
struct YieldFinder {
    control_flow_depth: usize,
    error: Option<syn::Error>,
}

impl YieldFinder {
    fn record(&mut self, e: syn::Error) {
        match &mut self.error {
            Some(prev) => prev.combine(e),
            None => self.error = Some(e),
        }
    }

    fn in_control_flow<F: FnOnce(&mut Self)>(&mut self, f: F) {
        self.control_flow_depth += 1;
        f(self);
        self.control_flow_depth -= 1;
    }
}

impl<'ast> Visit<'ast> for YieldFinder {
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if is_yield_macro(mac) {
            let msg = if self.control_flow_depth > 0 {
                "yield_! inside control flow (if/match/loop/while/for) is not supported in v1 \
                 (planned for v2); only straight-line yields at the top level of the function \
                 body are allowed"
            } else {
                "yield_! is only allowed as a top-level statement of the function body: \
                 `yield_!(expr);` or `let x = yield_!(expr);`"
            };
            self.record(syn::Error::new_spanned(mac, msg));
        } else if tokens_contain_yield(mac.tokens.clone()) {
            self.record(syn::Error::new(
                mac.span(),
                "yield_! cannot appear inside another macro invocation",
            ));
        }
    }

    fn visit_expr_if(&mut self, node: &'ast syn::ExprIf) {
        self.in_control_flow(|v| syn::visit::visit_expr_if(v, node));
    }

    fn visit_expr_match(&mut self, node: &'ast syn::ExprMatch) {
        self.in_control_flow(|v| syn::visit::visit_expr_match(v, node));
    }

    fn visit_expr_loop(&mut self, node: &'ast syn::ExprLoop) {
        self.in_control_flow(|v| syn::visit::visit_expr_loop(v, node));
    }

    fn visit_expr_while(&mut self, node: &'ast syn::ExprWhile) {
        self.in_control_flow(|v| syn::visit::visit_expr_while(v, node));
    }

    fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
        self.in_control_flow(|v| syn::visit::visit_expr_for_loop(v, node));
    }

    // Closures, async blocks, and nested items are separate scopes: a
    // yield_! inside them is not ours to transform, so they pass through.
    fn visit_expr_closure(&mut self, _: &'ast syn::ExprClosure) {}
    fn visit_expr_async(&mut self, _: &'ast syn::ExprAsync) {}
    fn visit_item(&mut self, _: &'ast syn::Item) {}
}

/// Textually scans a foreign macro's tokens for `yield_ !`.
fn tokens_contain_yield(tokens: proc_macro2::TokenStream) -> bool {
    let mut iter = tokens.into_iter().peekable();
    while let Some(tt) = iter.next() {
        match tt {
            proc_macro2::TokenTree::Ident(id) if id == "yield_" => {
                if matches!(
                    iter.peek(),
                    Some(proc_macro2::TokenTree::Punct(p)) if p.as_char() == '!'
                ) {
                    return true;
                }
            }
            proc_macro2::TokenTree::Group(g) => {
                if tokens_contain_yield(g.stream()) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    #[test]
    fn no_yield_is_single_segment() {
        let block: syn::Block = parse_quote!({
            let x = 1;
            x + 1
        });
        let ir = parse_body(&block).unwrap();
        assert_eq!(ir.segments.len(), 1);
        assert_eq!(ir.yields.len(), 0);
        assert_eq!(ir.segments[0].stmts.len(), 2);
    }

    #[test]
    fn yields_split_segments() {
        let block: syn::Block = parse_quote!({
            let a = 1;
            yield_!(a);
            let r = yield_!(2);
            r
        });
        let ir = parse_body(&block).unwrap();
        assert_eq!(ir.segments.len(), 3);
        assert_eq!(ir.yields.len(), 2);
        assert_eq!(ir.segments[0].stmts.len(), 1);
        assert_eq!(ir.segments[1].stmts.len(), 0);
        assert_eq!(ir.segments[2].stmts.len(), 1);
        assert!(ir.yields[0].resume_binding.is_none());
        let binding = ir.yields[1].resume_binding.as_ref().unwrap();
        assert_eq!(binding.ident, "r");
        assert!(binding.ty.is_none());
    }

    #[test]
    fn typed_resume_binding() {
        let block: syn::Block = parse_quote!({
            let mut r: String = yield_!(1);
        });
        let ir = parse_body(&block).unwrap();
        let binding = ir.yields[0].resume_binding.as_ref().unwrap();
        assert_eq!(binding.ident, "r");
        assert!(binding.mutability.is_some());
        assert!(binding.ty.is_some());
    }

    #[test]
    fn empty_yield_value_is_unit() {
        let block: syn::Block = parse_quote!({
            yield_!();
        });
        let ir = parse_body(&block).unwrap();
        let unit: syn::Expr = parse_quote!(());
        assert_eq!(ir.yields[0].value, unit);
    }

    #[test]
    fn yield_in_expression_is_rejected() {
        let block: syn::Block = parse_quote!({
            f(1, yield_!(2));
        });
        let err = parse_body(&block).unwrap_err();
        assert!(err.to_string().contains("top-level statement"));
    }

    #[test]
    fn yield_in_control_flow_is_rejected() {
        let block: syn::Block = parse_quote!({
            if cond {
                yield_!(1);
            }
        });
        let err = parse_body(&block).unwrap_err();
        assert!(err.to_string().contains("v2"));
    }

    #[test]
    fn yield_in_closure_passes_through() {
        let block: syn::Block = parse_quote!({
            let f = || yield_!(1);
        });
        let ir = parse_body(&block).unwrap();
        assert_eq!(ir.yields.len(), 0);
        assert_eq!(ir.segments.len(), 1);
    }

    #[test]
    fn yield_in_foreign_macro_is_rejected() {
        let block: syn::Block = parse_quote!({
            println!("{}", yield_!(1));
        });
        let err = parse_body(&block).unwrap_err();
        assert!(err.to_string().contains("another macro"));
    }

    #[test]
    fn multiple_errors_are_combined() {
        let block: syn::Block = parse_quote!({
            f(yield_!(1));
            g(yield_!(2));
        });
        let err = parse_body(&block).unwrap_err();
        assert_eq!(err.into_iter().count(), 2);
    }
}
