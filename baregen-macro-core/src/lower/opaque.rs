//! Opaque statement validation and jump rewriting: `break`/`continue`
//! escaping an opaque statement into an expanded loop or labeled block
//! are rewritten into `__baregen_jump!` markers.

use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::visit_mut::VisitMut;

use crate::cfg::{BindingId, BlockId, OpaqueJump, OpaqueJumpKind, TySource};

use super::visitors::{is_yield_macro, skip_nested_scopes, tokens_contain_yield};
use super::{BreakDest, ERR_BREAK_VALUE, ERR_FOREIGN_MACRO, ERR_UNHOISTABLE, Lowerer, unit_expr};

impl Lowerer {
    /// Checks and rewrites a statement that stays opaque: no foreign
    /// macro may carry yield_! tokens, and every `break`/`continue`
    /// inside it that targets an expanded (yield-containing) loop or
    /// labeled block outside it is rewritten into a `__baregen_jump!`
    /// marker recorded in `opaque_jumps`. Codegen later replaces the
    /// marker with a transition into the dispatch loop (or a completion
    /// for a valued `break` out of a tail-position loop), so the jump
    /// works at any depth inside the statement. The jump does not end
    /// the current block; its edge is carried by `Block::jumps`.
    pub(super) fn rewrite_opaque(&mut self, stmt: &syn::Stmt) -> syn::Stmt {
        let mut bound = StmtBindingCollector::default();
        bound.visit_stmt(stmt);
        let mut stmt = stmt.clone();
        let mut rw = OpaqueRewriter {
            lw: self,
            stmt_bindings: bound.idents,
            local_loop_depth: 0,
            local_labels: Vec::new(),
        };
        rw.visit_stmt_mut(&mut stmt);
        stmt
    }
}

/// Identifiers bound by patterns within one statement (`let`s, match
/// arms, `if let`/`while let` patterns, `for` patterns), nested scopes
/// excluded. Recorded per jump for the analysis' shadowing check.
#[derive(Default)]
struct StmtBindingCollector {
    idents: Vec<syn::Ident>,
}

impl<'ast> Visit<'ast> for StmtBindingCollector {
    fn visit_pat_ident(&mut self, pi: &'ast syn::PatIdent) {
        self.idents.push(pi.ident.clone());
        syn::visit::visit_pat_ident(self, pi);
    }

    skip_nested_scopes!(Visit);
}

/// Rewrites the jumps escaping one opaque statement. Mirrors the label
/// resolution of `lower_break`/`lower_continue`: loops and labels
/// declared within the statement own their own jumps and are tracked so
/// only escaping jumps are rewritten (and escaping jumps resolve against
/// the expanded frames, which are the only enclosing constructs — every
/// non-expanded one is inside the opaque statement itself).
struct OpaqueRewriter<'a> {
    lw: &'a mut Lowerer,
    /// See `StmtBindingCollector`.
    stmt_bindings: Vec<syn::Ident>,
    /// Loops of the statement itself that enclose the current node.
    local_loop_depth: usize,
    /// Labels declared within the statement (shadow expanded ones).
    local_labels: Vec<String>,
}

impl OpaqueRewriter<'_> {
    fn enter_loop<F: FnOnce(&mut Self)>(&mut self, label: &Option<syn::Label>, f: F) {
        let labeled = label.is_some();
        if let Some(l) = label {
            self.local_labels.push(l.name.ident.to_string());
        }
        self.local_loop_depth += 1;
        f(self);
        self.local_loop_depth -= 1;
        if labeled {
            self.local_labels.pop();
        }
    }

    /// Resolves a jump's frame like `lower_break` does, returning `None`
    /// both when the jump belongs to the statement itself (nothing to
    /// rewrite) and on a reported resolution error.
    fn escaping_frame(
        &mut self,
        label: &Option<syn::Lifetime>,
        kw: &str,
        span_of: &dyn quote::ToTokens,
    ) -> Option<(Option<BlockId>, BreakDest)> {
        let frame = match label {
            Some(l) => {
                if self.local_labels.contains(&l.ident.to_string()) {
                    return None;
                }
                let Some(f) = self.lw.find_labeled_frame(&l.ident.to_string()) else {
                    self.lw.err(syn::Error::new_spanned(
                        span_of,
                        format!("use of undeclared label `{l}`"),
                    ));
                    return None;
                };
                f
            }
            None => {
                if self.local_loop_depth > 0 {
                    return None;
                }
                let Some(f) = self.lw.innermost_loop_frame() else {
                    self.lw.err(syn::Error::new_spanned(
                        span_of,
                        format!("`{kw}` outside of a loop"),
                    ));
                    return None;
                };
                f
            }
        };
        Some((frame.header, frame.dest))
    }

    /// Allocates a jump entry owned by the current block and returns the
    /// bare `k` literal for its marker.
    fn new_jump(&mut self, kind: OpaqueJumpKind, span: proc_macro2::Span) -> syn::LitInt {
        let k = self.lw.opaque_jumps.len();
        self.lw.opaque_jumps.push(OpaqueJump {
            kind,
            shadowed: self.stmt_bindings.clone(),
        });
        let current = self.lw.current;
        self.lw.blocks[current].jumps.push(k);
        syn::LitInt::new(&k.to_string(), span)
    }

    /// The `{ __state = ..; continue '__dispatch; }` marker jumping to
    /// `target`.
    fn goto_marker(
        &mut self,
        target: BlockId,
        store: Option<BindingId>,
        span: proc_macro2::Span,
    ) -> syn::Expr {
        let k = self.new_jump(OpaqueJumpKind::Goto { target, store }, span);
        syn::parse_quote_spanned!(span => __baregen_jump!(#k))
    }

    /// The replacement for an escaping `break`, or `None` to keep it.
    fn rewrite_break(&mut self, b: &syn::ExprBreak) -> Option<syn::Expr> {
        let (_, dest) = self.escaping_frame(&b.label, "break", b)?;
        let span = b.break_token.span();
        let value = |b: &syn::ExprBreak| -> syn::Expr {
            b.expr
                .as_deref()
                .cloned()
                .unwrap_or_else(|| unit_expr(span))
        };
        match dest {
            BreakDest::Plain(exit) => {
                if let Some(v) = &b.expr {
                    self.lw.err(syn::Error::new_spanned(v, ERR_BREAK_VALUE));
                }
                Some(self.goto_marker(exit, None, span))
            }
            // Like the native `push_store`, but the synthesized `let`
            // sits at the jump site, inside the statement; the marker
            // right after it moves the fresh binding into the target
            // state (so a same-named user binding in the statement
            // cannot be captured by mistake).
            BreakDest::Store { binding, exit } => {
                let v = value(b);
                let bd = &self.lw.bindings[binding.0];
                let ident = bd.ident.clone();
                let mutability = bd.mutability;
                let annotation = match &bd.ty {
                    TySource::Known(t) => Some(t.clone()),
                    _ => None,
                };
                let current = self.lw.current;
                self.lw.blocks[current].defs.insert(binding);
                let marker = self.goto_marker(exit, Some(binding), span);
                let let_stmt: syn::Stmt = match annotation {
                    Some(ty) => syn::parse_quote_spanned!(span =>
                        let #mutability #ident: #ty = #v;),
                    None => syn::parse_quote_spanned!(span => let #mutability #ident = #v;),
                };
                Some(syn::parse_quote_spanned!(span => { #let_stmt #marker }))
            }
            // The loop is the function's tail expression: the `break`
            // value completes the coroutine. The marker carries the
            // value; codegen wraps it in the completion transition
            // (only it knows the return type).
            BreakDest::Return => {
                let v = value(b);
                let k = self.new_jump(OpaqueJumpKind::Complete, span);
                Some(syn::parse_quote_spanned!(span => __baregen_jump!(#k, #v)))
            }
        }
    }

    /// The replacement for an escaping `continue`, or `None` to keep it.
    fn rewrite_continue(&mut self, c: &syn::ExprContinue) -> Option<syn::Expr> {
        let (header, _) = self.escaping_frame(&c.label, "continue", c)?;
        let Some(header) = header else {
            self.lw.err(syn::Error::new_spanned(
                c,
                "`continue` cannot target a labeled block",
            ));
            return None;
        };
        Some(self.goto_marker(header, None, c.continue_token.span()))
    }
}

impl VisitMut for OpaqueRewriter<'_> {
    fn visit_expr_mut(&mut self, e: &mut syn::Expr) {
        // Children first: a jump nested inside a `break` value must be
        // rewritten before the outer break captures the value.
        syn::visit_mut::visit_expr_mut(self, e);
        let replacement = match e {
            syn::Expr::Break(b) => self.rewrite_break(b),
            syn::Expr::Continue(c) => self.rewrite_continue(c),
            _ => None,
        };
        if let Some(r) = replacement {
            *e = r;
        }
    }

    fn visit_macro_mut(&mut self, mac: &mut syn::Macro) {
        if is_yield_macro(mac) {
            // Unreachable in practice: the caller only rewrites
            // statements without yield_!. Kept for robustness.
            self.lw.err(syn::Error::new_spanned(&*mac, ERR_UNHOISTABLE));
        } else if tokens_contain_yield(mac.tokens.clone()) {
            self.lw.err(syn::Error::new(mac.span(), ERR_FOREIGN_MACRO));
        }
    }

    fn visit_expr_loop_mut(&mut self, node: &mut syn::ExprLoop) {
        let label = node.label.clone();
        self.enter_loop(&label, |v| syn::visit_mut::visit_expr_loop_mut(v, node));
    }

    fn visit_expr_while_mut(&mut self, node: &mut syn::ExprWhile) {
        let label = node.label.clone();
        self.enter_loop(&label, |v| syn::visit_mut::visit_expr_while_mut(v, node));
    }

    fn visit_expr_for_loop_mut(&mut self, node: &mut syn::ExprForLoop) {
        let label = node.label.clone();
        self.enter_loop(&label, |v| syn::visit_mut::visit_expr_for_loop_mut(v, node));
    }

    fn visit_expr_block_mut(&mut self, node: &mut syn::ExprBlock) {
        if let Some(l) = &node.label {
            self.local_labels.push(l.name.ident.to_string());
            syn::visit_mut::visit_expr_block_mut(self, node);
            self.local_labels.pop();
        } else {
            syn::visit_mut::visit_expr_block_mut(self, node);
        }
    }

    // Separate scopes: break/continue and yield_! inside these belong to
    // them, not to the coroutine.
    skip_nested_scopes!(VisitMut);
}
