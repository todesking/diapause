//! Pre-lowering hoisting of expression-position `yield_!`.
//!
//! A `yield_!` inside an expression is supported when it sits in an
//! evaluation-order prefix of its statement: everything evaluated
//! before it must be a path, a literal, or another `yield_!`. Each such
//! yield is hoisted into its own `let __tmpN = yield_!(..);` statement
//! in front of the enclosing statement, and the expression refers to
//! `__tmpN` instead; lowering then sees only the natively supported
//! statement forms. Anything effectful or panicking (a call, indexing,
//! arithmetic, ...) evaluated before a yield would be reordered across
//! the suspension by such a move, so those yields are left in place for
//! lowering to reject.
//!
//! Positions hoisted from: expression statements, `let` initializers
//! (including `let ... else` scrutinees), trailing expressions,
//! non-block match-arm bodies, `if` conditions (the first link of an
//! `else if` chain only — later links are conditionally evaluated),
//! `match`/`if let` scrutinees, and `for` heads. `while` conditions and
//! `while let` scrutinees are re-evaluated every iteration and are
//! never hoisted. Conditionally evaluated positions (`&&`/`||` right
//! operands, match guards, control flow nested inside an expression)
//! and separate scopes (closures, async blocks, items) are skipped.
//!
//! The `__tmpN` binding is an ordinary resume binding, so its type
//! defaults to the attribute's resume type when it crosses a later
//! yield (as in `f(yield_!(1), yield_!(2))`).

use quote::ToTokens;
use syn::spanned::Spanned;
use syn::visit_mut::VisitMut;

use crate::lower::{is_let_chain, is_yield_all_macro, is_yield_macro, skip_nested_scopes};

/// Rewrites hoistable expression-position yields in `body` (see the
/// module docs). Runs before `rewrite_early_exits`, so `?` operators
/// are still visible as `Expr::Try` (an effect that ends the prefix).
pub fn hoist_yields(body: &mut syn::Block) {
    let mut h = Hoister {
        tmp_count: 0,
        prefix: Vec::new(),
        pure: true,
    };
    h.visit_block_mut(body);
}

struct Hoister {
    /// Number of `__tmp{k}` bindings created so far (per body).
    tmp_count: usize,
    /// Hoisted `let __tmp{k} = yield_!(..);` statements for the
    /// statement currently being processed.
    prefix: Vec<syn::Stmt>,
    /// Whether everything evaluated so far in the current statement is
    /// a path, a literal, or a hoisted yield. Monotone within a
    /// statement: once false, nothing hoists.
    pure: bool,
}

impl Hoister {
    /// Hoists from one statement's top-level positions, returning the
    /// `let` statements to insert in front of it. Nested blocks are
    /// handled by the `VisitMut` recursion, not here.
    fn hoist_stmt(&mut self, stmt: &mut syn::Stmt) -> Vec<syn::Stmt> {
        self.pure = true;
        debug_assert!(self.prefix.is_empty(), "BUG: prefix not drained");
        match stmt {
            // A bare trailing `yield_!(..)` produces the resume value:
            // bind it and make the binding the tail expression.
            syn::Stmt::Macro(sm) if is_yield_macro(&sm.mac) && sm.semi_token.is_none() => {
                let mut e = syn::Expr::Macro(syn::ExprMacro {
                    attrs: std::mem::take(&mut sm.attrs),
                    mac: sm.mac.clone(),
                });
                self.walk(&mut e);
                *stmt = syn::Stmt::Expr(e, None);
            }
            // `yield_!(..);` is native; foreign macros are opaque.
            syn::Stmt::Macro(_) => {}
            syn::Stmt::Local(local) => {
                if let Some(init) = &mut local.init {
                    // `let x = yield_!(..);` / `let x = yield_all!(..);`
                    // are the natively supported forms; `let ... else`
                    // is not, so its initializer is hoisted like any
                    // other expression.
                    let native = init.diverge.is_none()
                        && matches!(&*init.expr, syn::Expr::Macro(m)
                            if is_yield_macro(&m.mac) || is_yield_all_macro(&m.mac));
                    if !native {
                        self.head(&mut init.expr);
                    }
                }
            }
            syn::Stmt::Expr(e, semi) => match e {
                syn::Expr::Macro(em) if is_yield_macro(&em.mac) => {
                    // `yield_!(..);` is native; a trailing one is bound
                    // like the `Stmt::Macro` form above.
                    if semi.is_none() {
                        self.walk(e);
                    }
                }
                // `yield_all!(..)` is never hoisted; its positions are
                // validated by lowering.
                syn::Expr::Macro(_) => {}
                e => self.head(e),
            },
            _ => {}
        }
        std::mem::take(&mut self.prefix)
    }

    /// Hoists from a statement-level expression: control-flow
    /// expressions expose only their once-evaluated head position
    /// (condition, scrutinee, `for` head); anything else is walked
    /// whole.
    fn head(&mut self, e: &mut syn::Expr) {
        match e {
            syn::Expr::If(ei) => match &mut *ei.cond {
                // Let chains are rejected wholesale by lowering.
                c if is_let_chain(c) => {}
                syn::Expr::Let(el) => self.walk(&mut el.expr),
                c => self.walk(c),
            },
            syn::Expr::Match(em) => self.walk(&mut em.expr),
            syn::Expr::ForLoop(ef) => self.walk(&mut ef.expr),
            // A `while` condition / `while let` scrutinee is
            // re-evaluated every iteration; the rest have no head, only
            // nested blocks (handled by the visitor recursion).
            syn::Expr::While(_)
            | syn::Expr::Loop(_)
            | syn::Expr::Block(_)
            | syn::Expr::Unsafe(_)
            | syn::Expr::TryBlock(_)
            | syn::Expr::Const(_) => {}
            other => self.walk(other),
        }
    }

    /// Walks an expression in evaluation order, hoisting every yield
    /// reached while `self.pure` still holds.
    fn walk(&mut self, e: &mut syn::Expr) {
        match e {
            syn::Expr::Macro(em) if is_yield_macro(&em.mac) => {
                if self.pure {
                    self.hoist(e);
                }
            }
            // `yield_all!` is never hoisted and is effectful (as is any
            // foreign macro), so nothing after it hoists either.
            syn::Expr::Macro(_) => self.pure = false,
            syn::Expr::Path(_) | syn::Expr::Lit(_) => {}
            syn::Expr::Paren(p) => self.walk(&mut p.expr),
            syn::Expr::Group(g) => self.walk(&mut g.expr),
            syn::Expr::Binary(b) if matches!(b.op, syn::BinOp::And(_) | syn::BinOp::Or(_)) => {
                self.walk(&mut b.left);
                // The right operand is conditionally evaluated.
                self.pure = false;
            }
            syn::Expr::Binary(b) if is_assign_op(&b.op) => {
                // Compound assignment: a primitive `+=` evaluates
                // right-then-left, an overloaded `AddAssign` left-then-
                // right; restricting the place to a plain identifier
                // (pure under either order) keeps the right operand
                // hoistable regardless.
                if is_ident_path(&b.left) {
                    self.walk(&mut b.right);
                }
                self.pure = false;
            }
            syn::Expr::Binary(b) => {
                self.walk(&mut b.left);
                self.walk(&mut b.right);
                // The operation itself may panic (overflow, division).
                self.pure = false;
            }
            // `place = value` evaluates the value first, then the place.
            syn::Expr::Assign(a) => {
                self.walk(&mut a.right);
                self.walk(&mut a.left);
                self.pure = false;
            }
            syn::Expr::Unary(u) => {
                self.walk(&mut u.expr);
                self.pure = false;
            }
            syn::Expr::Call(c) => {
                self.walk(&mut c.func);
                for arg in &mut c.args {
                    self.walk(arg);
                }
                self.pure = false;
            }
            syn::Expr::MethodCall(mc) => {
                self.walk(&mut mc.receiver);
                // Adjusting the receiver may run user `Deref` code
                // (autoderef) before the arguments, so the arguments
                // never hoist.
                self.pure = false;
            }
            syn::Expr::Index(i) => {
                self.walk(&mut i.expr);
                self.walk(&mut i.index);
                self.pure = false;
            }
            syn::Expr::Field(f) => {
                self.walk(&mut f.base);
                self.pure = false;
            }
            syn::Expr::Tuple(t) => {
                for elem in &mut t.elems {
                    self.walk(elem);
                }
                self.pure = false;
            }
            syn::Expr::Array(a) => {
                for elem in &mut a.elems {
                    self.walk(elem);
                }
                self.pure = false;
            }
            syn::Expr::Repeat(r) => {
                self.walk(&mut r.expr);
                self.pure = false;
            }
            syn::Expr::Struct(s) => {
                for field in &mut s.fields {
                    self.walk(&mut field.expr);
                }
                if let Some(rest) = &mut s.rest {
                    self.walk(rest);
                }
                self.pure = false;
            }
            syn::Expr::Range(r) => {
                if let Some(start) = &mut r.start {
                    self.walk(start);
                }
                if let Some(end) = &mut r.end {
                    self.walk(end);
                }
                self.pure = false;
            }
            syn::Expr::Cast(c) => {
                self.walk(&mut c.expr);
                self.pure = false;
            }
            syn::Expr::Try(t) => {
                self.walk(&mut t.expr);
                self.pure = false;
            }
            syn::Expr::Reference(r) => {
                self.walk(&mut r.expr);
                self.pure = false;
            }
            // Everything else is conditionally/repeatedly evaluated
            // (control flow), diverges, or is a separate scope; yields
            // inside are left for lowering to reject.
            _ => self.pure = false,
        }
    }

    /// Replaces a `yield_!` expression with a fresh `__tmp{k}` path and
    /// records `let __tmp{k} = yield_!(..);` in the prefix.
    fn hoist(&mut self, e: &mut syn::Expr) {
        let syn::Expr::Macro(em) = e else {
            unreachable!("BUG: checked by the caller")
        };
        // Nested yields inside the value are evaluated before this one
        // and move out with it, so hoist them first. Effects inside the
        // value keep their order relative to every hoisted yield and do
        // not end the prefix.
        if !em.mac.tokens.is_empty()
            && let Ok(mut value) = em.mac.parse_body::<syn::Expr>()
        {
            let saved = self.pure;
            self.walk(&mut value);
            self.pure = saved;
            em.mac.tokens = value.to_token_stream();
        }
        let span = em.mac.span();
        let ident = syn::Ident::new(&format!("__tmp{}", self.tmp_count), span);
        self.tmp_count += 1;
        let mac = &em.mac;
        self.prefix
            .push(syn::parse_quote_spanned!(span => let #ident = #mac;));
        *e = syn::parse_quote_spanned!(span => #ident);
    }
}

impl VisitMut for Hoister {
    fn visit_block_mut(&mut self, block: &mut syn::Block) {
        let mut out = Vec::with_capacity(block.stmts.len());
        for mut stmt in std::mem::take(&mut block.stmts) {
            let prefix = self.hoist_stmt(&mut stmt);
            out.extend(prefix);
            // Recurse after hoisting: nested blocks (branch bodies,
            // loop bodies, ...) get their own statements hoisted.
            syn::visit_mut::visit_stmt_mut(self, &mut stmt);
            out.push(stmt);
        }
        block.stmts = out;
    }

    /// A non-block match-arm body is a one-statement block in spirit:
    /// hoisting inside it wraps it into a real block so the `let`s have
    /// somewhere to go (they stay conditional on the arm being chosen).
    fn visit_arm_mut(&mut self, arm: &mut syn::Arm) {
        syn::visit_mut::visit_arm_mut(self, arm);
        if matches!(&*arm.body, syn::Expr::Block(_)) {
            return;
        }
        let body = std::mem::replace(
            &mut *arm.body,
            syn::Expr::Verbatim(proc_macro2::TokenStream::new()),
        );
        let mut stmt = syn::Stmt::Expr(body, None);
        let prefix = self.hoist_stmt(&mut stmt);
        let syn::Stmt::Expr(body, None) = stmt else {
            unreachable!("BUG: hoist_stmt keeps tail expressions tails")
        };
        *arm.body = if prefix.is_empty() {
            body
        } else {
            syn::parse_quote!({ #(#prefix)* #body })
        };
    }

    skip_nested_scopes!(VisitMut);
}

fn is_assign_op(op: &syn::BinOp) -> bool {
    use syn::BinOp::*;
    matches!(
        op,
        AddAssign(_)
            | SubAssign(_)
            | MulAssign(_)
            | DivAssign(_)
            | RemAssign(_)
            | BitXorAssign(_)
            | BitAndAssign(_)
            | BitOrAssign(_)
            | ShlAssign(_)
            | ShrAssign(_)
    )
}

fn is_ident_path(e: &syn::Expr) -> bool {
    matches!(e, syn::Expr::Path(p) if p.qself.is_none() && p.path.get_ident().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;
    use syn::parse_quote;

    fn hoisted(mut block: syn::Block) -> String {
        hoist_yields(&mut block);
        quote!(#block).to_string()
    }

    fn assert_unchanged(block: syn::Block) {
        let before = quote!(#block).to_string();
        assert_eq!(hoisted(block), before);
    }

    fn assert_hoists(input: syn::Block, expected: syn::Block) {
        assert_eq!(hoisted(input), quote!(#expected).to_string());
    }

    // === Hoisted forms ===

    #[test]
    fn call_arguments_with_a_yield_prefix() {
        assert_hoists(
            parse_quote!({
                f(yield_!(1), yield_!(2), foo(), bar());
            }),
            parse_quote!({
                let __tmp0 = yield_!(1);
                let __tmp1 = yield_!(2);
                f(__tmp0, __tmp1, foo(), bar());
            }),
        );
    }

    #[test]
    fn binary_left_operand() {
        assert_hoists(
            parse_quote!({
                let x = yield_!(1) + 2;
            }),
            parse_quote!({
                let __tmp0 = yield_!(1);
                let x = __tmp0 + 2;
            }),
        );
    }

    #[test]
    fn assignment_rhs() {
        assert_hoists(
            parse_quote!({
                x = f(yield_!(1));
            }),
            parse_quote!({
                let __tmp0 = yield_!(1);
                x = f(__tmp0);
            }),
        );
    }

    #[test]
    fn plain_assignment_of_a_yield() {
        assert_hoists(
            parse_quote!({
                x = yield_!(1);
            }),
            parse_quote!({
                let __tmp0 = yield_!(1);
                x = __tmp0;
            }),
        );
    }

    #[test]
    fn compound_assignment_to_an_identifier() {
        assert_hoists(
            parse_quote!({
                x += yield_!(1);
            }),
            parse_quote!({
                let __tmp0 = yield_!(1);
                x += __tmp0;
            }),
        );
    }

    #[test]
    fn trailing_yield_becomes_a_bound_tail() {
        assert_hoists(
            parse_quote!({
                yield_!(1)
            }),
            parse_quote!({
                let __tmp0 = yield_!(1);
                __tmp0
            }),
        );
    }

    #[test]
    fn trailing_expression_with_a_yield_prefix() {
        assert_hoists(
            parse_quote!({
                f(yield_!(1))
            }),
            parse_quote!({
                let __tmp0 = yield_!(1);
                f(__tmp0)
            }),
        );
    }

    #[test]
    fn if_condition() {
        assert_hoists(
            parse_quote!({
                if f(yield_!(1)) {
                    g();
                }
            }),
            parse_quote!({
                let __tmp0 = yield_!(1);
                if f(__tmp0) {
                    g();
                }
            }),
        );
    }

    #[test]
    fn if_let_scrutinee() {
        assert_hoists(
            parse_quote!({
                if let Some(x) = f(yield_!(1)) {
                    g(x);
                }
            }),
            parse_quote!({
                let __tmp0 = yield_!(1);
                if let Some(x) = f(__tmp0) {
                    g(x);
                }
            }),
        );
    }

    #[test]
    fn match_scrutinee() {
        assert_hoists(
            parse_quote!({
                match g(yield_!(1)) {
                    _ => {}
                }
            }),
            parse_quote!({
                let __tmp0 = yield_!(1);
                match g(__tmp0) {
                    _ => {}
                }
            }),
        );
    }

    #[test]
    fn for_head() {
        assert_hoists(
            parse_quote!({
                for x in g(yield_!(1)) {
                    h(x);
                }
            }),
            parse_quote!({
                let __tmp0 = yield_!(1);
                for x in g(__tmp0) {
                    h(x);
                }
            }),
        );
    }

    #[test]
    fn let_else_scrutinee() {
        assert_hoists(
            parse_quote!({
                let Some(x) = f(yield_!(1)) else {
                    return;
                };
            }),
            parse_quote!({
                let __tmp0 = yield_!(1);
                let Some(x) = f(__tmp0) else {
                    return;
                };
            }),
        );
    }

    #[test]
    fn nested_yields_hoist_inside_out() {
        assert_hoists(
            parse_quote!({
                f(yield_!(yield_!(1)));
            }),
            parse_quote!({
                let __tmp0 = yield_!(1);
                let __tmp1 = yield_!(__tmp0);
                f(__tmp1);
            }),
        );
    }

    #[test]
    fn effects_inside_a_yield_value_move_with_it() {
        assert_hoists(
            parse_quote!({
                g(yield_!(f()), yield_!(2));
            }),
            parse_quote!({
                let __tmp0 = yield_!(f());
                let __tmp1 = yield_!(2);
                g(__tmp0, __tmp1);
            }),
        );
    }

    #[test]
    fn method_call_receiver() {
        assert_hoists(
            parse_quote!({
                let s = yield_!(1).to_string();
            }),
            parse_quote!({
                let __tmp0 = yield_!(1);
                let s = __tmp0.to_string();
            }),
        );
    }

    #[test]
    fn bare_match_arm_body_is_wrapped() {
        assert_hoists(
            parse_quote!({
                match k {
                    0 => f(yield_!(1)),
                    _ => {}
                }
            }),
            parse_quote!({
                match k {
                    0 => {
                        let __tmp0 = yield_!(1);
                        f(__tmp0)
                    },
                    _ => {}
                }
            }),
        );
    }

    #[test]
    fn nested_statements_hoist_recursively() {
        assert_hoists(
            parse_quote!({
                if c {
                    g(yield_!(2));
                }
            }),
            parse_quote!({
                if c {
                    let __tmp0 = yield_!(2);
                    g(__tmp0);
                }
            }),
        );
    }

    #[test]
    fn tmp_numbering_is_global_within_a_body() {
        assert_hoists(
            parse_quote!({
                f(yield_!(1));
                g(yield_!(2));
            }),
            parse_quote!({
                let __tmp0 = yield_!(1);
                f(__tmp0);
                let __tmp1 = yield_!(2);
                g(__tmp1);
            }),
        );
    }

    // === Native forms stay untouched ===

    #[test]
    fn statement_and_let_yields_are_untouched() {
        assert_unchanged(parse_quote!({
            yield_!(1);
            let x = yield_!(2);
            let y: u32 = yield_all!(g);
            yield_all!(h);
        }));
    }

    // === Impure prefixes stay untouched ===

    #[test]
    fn call_before_the_yield_blocks_hoisting() {
        assert_unchanged(parse_quote!({
            f(g(), yield_!(2));
        }));
    }

    #[test]
    fn path_operand_before_the_yield_is_pure() {
        assert_hoists(
            parse_quote!({
                let x = a + yield_!(1);
            }),
            parse_quote!({
                let __tmp0 = yield_!(1);
                let x = a + __tmp0;
            }),
        );
    }

    #[test]
    fn call_operand_before_the_yield_blocks_hoisting() {
        assert_unchanged(parse_quote!({
            let x = f() + yield_!(1);
        }));
    }

    #[test]
    fn try_before_the_yield_blocks_hoisting() {
        assert_unchanged(parse_quote!({
            f(x?, yield_!(1));
        }));
    }

    #[test]
    fn yield_all_blocks_later_hoisting() {
        assert_unchanged(parse_quote!({
            f(yield_all!(g), yield_!(1));
        }));
    }

    #[test]
    fn compound_assignment_to_a_complex_place_is_untouched() {
        assert_unchanged(parse_quote!({
            a[i()] += yield_!(1);
        }));
    }

    #[test]
    fn method_call_arguments_never_hoist() {
        assert_unchanged(parse_quote!({
            recv.m(yield_!(1));
        }));
    }

    // === Conditional / repeated evaluation stays untouched ===

    #[test]
    fn short_circuit_rhs_never_hoists() {
        assert_unchanged(parse_quote!({
            let x = c && yield_!(1);
        }));
    }

    #[test]
    fn while_condition_never_hoists() {
        assert_unchanged(parse_quote!({
            while f(yield_!(1)) {
                g();
            }
        }));
    }

    #[test]
    fn while_let_scrutinee_never_hoists() {
        assert_unchanged(parse_quote!({
            while let Some(x) = f(yield_!(1)) {
                g(x);
            }
        }));
    }

    #[test]
    fn else_if_condition_never_hoists() {
        assert_unchanged(parse_quote!({
            if a {
                f();
            } else if g(yield_!(1)) {
                h();
            }
        }));
    }

    #[test]
    fn match_guard_never_hoists() {
        assert_unchanged(parse_quote!({
            match v {
                y if f(yield_!(y)) => {}
                _ => {}
            }
        }));
    }

    #[test]
    fn closures_are_untouched() {
        assert_unchanged(parse_quote!({
            let f = |x: u32| g(yield_!(x));
        }));
    }

    #[test]
    fn nested_control_flow_in_an_expression_never_hoists() {
        assert_unchanged(parse_quote!({
            let x = 1 + if c { yield_!(1); 1 } else { 2 };
        }));
    }

    #[test]
    fn foreign_macros_are_untouched() {
        assert_unchanged(parse_quote!({
            println!("{}", yield_!(1));
        }));
    }
}
