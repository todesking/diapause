//! Lowers a coroutine body into a control-flow graph.
//!
//! Only statements that transitively contain `yield_!` are expanded into
//! CFG structure; every other statement — control flow included — is kept
//! as an opaque statement inside a basic block.

use std::collections::{BTreeSet, HashMap, HashSet};

use syn::spanned::Spanned;
use syn::visit::Visit;

use crate::cfg::{
    Binding, BindingId, BindingKind, Block, BlockId, BorrowSource, Cfg, MatchArm, OpaqueJump,
    ResumeBinding, Terminator, TySource, simplify,
};
use crate::ty_infer::strip_parens;

mod opaque;
#[cfg(test)]
mod tests;
mod visitors;

pub(crate) use visitors::{
    PatBindingCollector, UseCollector, collect_markers, collect_token_idents, skip_nested_scopes,
};
use visitors::{YieldBan, contains_yield_expr, contains_yield_stmt};
pub use visitors::{is_jump_marker, is_yield_all_macro, is_yield_macro};

/// The syntactic form of a `yield_!` resume binding, before it has been
/// assigned a `BindingId`.
struct ResumeBindingSpec {
    ident: syn::Ident,
    mutability: Option<syn::Token![mut]>,
    ty: Option<syn::Type>,
}

/// What a yield does with its resume value.
enum ResumeTarget {
    /// `yield_!(expr);` — the resume value is dropped.
    Discard,
    /// `let x = yield_!(expr);` — a fresh binding. Boxed to keep the
    /// enum small (`ResumeBindingSpec` embeds a `syn::Type`).
    Bind(Box<ResumeBindingSpec>),
    /// `yield_all!`'s internal loop: the resume value re-defines the
    /// existing `__rv{k}` binding so it can be carried around the
    /// delegation loop's back edge without a reassignment (which the
    /// block-level use over-approximation would treat as a use, keeping
    /// the moved-out value live across the yield).
    Rebind(BindingId),
}

/// Accumulates zero or more `syn::Error`s, combining them in push order
/// so the emitted `compile_error!`s appear in the order the errors were
/// found.
#[derive(Default)]
pub struct ErrorSink {
    error: Option<syn::Error>,
    count: usize,
}

impl ErrorSink {
    pub fn push(&mut self, e: syn::Error) {
        match &mut self.error {
            Some(prev) => prev.combine(e),
            None => self.error = Some(e),
        }
        self.count += 1;
    }

    /// Number of errors pushed into the sink so far.
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn into_option(self) -> Option<syn::Error> {
        self.error
    }

    /// Consumes the sink: `Err` with the combined error if non-empty,
    /// `Ok(ok)` otherwise.
    pub fn into_result<T>(self, ok: T) -> syn::Result<T> {
        match self.error {
            Some(e) => Err(e),
            None => Ok(ok),
        }
    }
}

/// Lowers a coroutine body to a simplified CFG. `args` are the function
/// argument names; they become `BindingId(0)..` and are in scope from
/// the entry block without being defined by it. `arg_pats` are the
/// destructuring patterns of non-simple-identifier arguments, each
/// paired with the fresh argument ident holding its value; they become
/// `let <pat> = <ident>;` statements at the top of the entry block.
pub fn lower(
    args: &[syn::Ident],
    arg_pats: &[(syn::Pat, syn::Ident)],
    body: &syn::Block,
) -> syn::Result<Cfg> {
    let mut cfg = lower_unsimplified(args, arg_pats, body)?;
    simplify(&mut cfg);
    Ok(cfg)
}

/// Same as [`lower`] but without the simplification pass: every block is
/// as the lowering built it (no goto-chain merging, no inlining). Used
/// by `expand_debug` to snapshot the pre-simplification CFG.
pub fn lower_unsimplified(
    args: &[syn::Ident],
    arg_pats: &[(syn::Pat, syn::Ident)],
    body: &syn::Block,
) -> syn::Result<Cfg> {
    let mut lw = Lowerer::new(args);
    for (pat, source) in arg_pats {
        lw.push_arg_pat_let(pat, source);
    }
    lw.lower_fn_body(body);
    lw.finish()
}

#[derive(PartialEq, Clone, Copy)]
enum TailCtx {
    /// The trailing expression is the coroutine's return value.
    FnReturn,
    /// The trailing expression's value is discarded (statement blocks,
    /// loop bodies, match arms).
    Discard,
    /// The trailing expression's value is assigned to a `let` binding
    /// (the initializer of `let x = if/match/loop/{..}` and, recursively,
    /// the arms thereof).
    Store(BindingId),
}

struct DraftBlock {
    stmts: Vec<syn::Stmt>,
    terminator: Option<Terminator>,
    uses: BTreeSet<BindingId>,
    defs: BTreeSet<BindingId>,
    resume_point: bool,
    jumps: Vec<usize>,
}

/// A `break`/`continue` target introduced by an expanded loop or an
/// expanded labeled block (`header` is `None` for the latter).
struct Frame {
    label: Option<String>,
    header: Option<BlockId>,
    dest: BreakDest,
}

/// What a `break` targeting a frame does, determined by the position of
/// the loop or labeled block it belongs to.
#[derive(Clone, Copy)]
enum BreakDest {
    /// Statement position: jump to the exit block; a value is an error.
    Plain(BlockId),
    /// `let` initializer: assign the value to the binding, then jump.
    Store { binding: BindingId, exit: BlockId },
    /// The function's trailing expression: the value completes the
    /// coroutine.
    Return,
}

impl BreakDest {
    /// The destination for a value produced in the frame's position:
    /// loops pass it to their `break`s, labeled blocks additionally to
    /// their trailing expression.
    fn of(ctx: TailCtx, exit: BlockId) -> Self {
        match ctx {
            TailCtx::Discard => BreakDest::Plain(exit),
            TailCtx::Store(binding) => BreakDest::Store { binding, exit },
            TailCtx::FnReturn => BreakDest::Return,
        }
    }
}

/// `pub(crate)` so that `ty_infer.rs` can add an `impl Lowerer` block of
/// its own.
pub(crate) struct Lowerer {
    blocks: Vec<DraftBlock>,
    bindings: Vec<Binding>,
    opaque_jumps: Vec<OpaqueJump>,
    scopes: Vec<HashMap<String, BindingId>>,
    labels: Vec<Frame>,
    errors: ErrorSink,
    current: BlockId,
    /// Number of `__iter{k}` bindings created so far.
    for_count: usize,
    /// Number of `yield_all!` expansions so far (numbers `__dg{k}` etc.).
    yield_all_count: usize,
    /// Nonzero while lowering a `yield_all!` expansion; gates the
    /// internal `__rv{k} = yield_!(..);` rebind form, which is not part
    /// of the user-facing syntax.
    yield_all_depth: usize,
}

impl Lowerer {
    fn new(args: &[syn::Ident]) -> Self {
        let mut lw = Lowerer {
            blocks: Vec::new(),
            bindings: Vec::new(),
            opaque_jumps: Vec::new(),
            scopes: vec![HashMap::new()],
            labels: Vec::new(),
            errors: ErrorSink::default(),
            current: 0,
            for_count: 0,
            yield_all_count: 0,
            yield_all_depth: 0,
        };
        for arg in args {
            let id = BindingId(lw.bindings.len());
            lw.bindings.push(Binding {
                ident: arg.clone(),
                mutability: None,
                kind: BindingKind::Arg,
                ty: TySource::Unknown,
                borrow: BorrowSource::NotABorrow,
                def_stmt: None,
            });
            lw.scopes[0].insert(arg.to_string(), id);
        }
        let entry = lw.new_block(false);
        lw.current = entry;
        lw
    }

    fn lower_fn_body(&mut self, body: &syn::Block) {
        self.in_scope(|lw| lw.lower_stmt_list(&body.stmts, TailCtx::FnReturn));
    }

    /// Runs `f` with a fresh innermost scope, popping it afterwards
    /// regardless of what bindings `f` introduced.
    fn in_scope(&mut self, f: impl FnOnce(&mut Self)) {
        self.scopes.push(HashMap::new());
        f(self);
        self.scopes.pop();
    }

    fn finish(self) -> syn::Result<Cfg> {
        if let Some(e) = self.errors.into_option() {
            return Err(e);
        }
        let blocks = self
            .blocks
            .into_iter()
            .map(|d| Block {
                stmts: d.stmts,
                terminator: d.terminator.expect("BUG: unterminated block"),
                uses: d.uses,
                defs: d.defs,
                resume_point: d.resume_point,
                jumps: d.jumps,
                inline: false,
            })
            .collect();
        Ok(Cfg {
            blocks,
            entry: 0,
            bindings: self.bindings,
            opaque_jumps: self.opaque_jumps,
        })
    }

    fn err(&mut self, e: syn::Error) {
        self.errors.push(e);
    }

    fn new_block(&mut self, resume_point: bool) -> BlockId {
        self.blocks.push(DraftBlock {
            stmts: Vec::new(),
            terminator: None,
            uses: BTreeSet::new(),
            defs: BTreeSet::new(),
            resume_point,
            jumps: Vec::new(),
        });
        self.blocks.len() - 1
    }

    fn set_terminator(&mut self, block: BlockId, t: Terminator) {
        let b = &mut self.blocks[block];
        debug_assert!(b.terminator.is_none(), "BUG: block already terminated");
        b.terminator = Some(t);
    }

    fn terminate(&mut self, t: Terminator) {
        self.set_terminator(self.current, t);
    }

    fn is_current_terminated(&self) -> bool {
        self.blocks[self.current].terminator.is_some()
    }

    /// `pub(crate)` for `ty_infer.rs`'s inference and borrow-classification
    /// methods.
    pub(crate) fn resolve(&self, name: &str) -> Option<BindingId> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    /// Introduces a fresh binding into the innermost scope and records
    /// its definition in `block`. Mutability, type, and borrow details
    /// are filled in by the caller where known. `def_stmt` is the index
    /// of the `let` statement that introduces this binding within
    /// `block`'s `stmts`, if any.
    fn define(
        &mut self,
        ident: &syn::Ident,
        block: BlockId,
        kind: BindingKind,
        def_stmt: Option<usize>,
    ) -> BindingId {
        let id = BindingId(self.bindings.len());
        self.bindings.push(Binding {
            ident: ident.clone(),
            mutability: None,
            kind,
            ty: TySource::Unknown,
            borrow: BorrowSource::NotABorrow,
            def_stmt,
        });
        self.scopes
            .last_mut()
            .expect("BUG: no scope")
            .insert(ident.to_string(), id);
        self.blocks[block].defs.insert(id);
        id
    }

    /// Resolves collected names and records them as uses of `block`,
    /// skipping bindings the block itself already defined (a use after a
    /// local definition is not upward-exposed).
    fn commit_uses(&mut self, names: HashSet<String>, block: BlockId) {
        let resolved: Vec<BindingId> = names.iter().filter_map(|n| self.resolve(n)).collect();
        let b = &mut self.blocks[block];
        for id in resolved {
            if !b.defs.contains(&id) {
                b.uses.insert(id);
            }
        }
    }

    fn record_stmt_uses(&mut self, stmt: &syn::Stmt) {
        let mut c = UseCollector::default();
        c.visit_stmt(stmt);
        self.commit_uses(c.used, self.current);
    }

    fn record_expr_uses(&mut self, expr: &syn::Expr, block: BlockId) {
        let mut c = UseCollector::default();
        c.visit_expr(expr);
        self.commit_uses(c.used, block);
    }

    /// Introduces every identifier bound by `pat` as a fresh binding
    /// defined in `block`.
    fn define_pat_bindings(
        &mut self,
        pat: &syn::Pat,
        block: BlockId,
        kind: BindingKind,
        def_stmt: Option<usize>,
    ) {
        let mut c = PatBindingCollector::default();
        c.visit_pat(pat);
        for (ident, mutability) in c.bindings {
            let id = self.define(&ident, block, kind, def_stmt);
            self.bindings[id.0].mutability = mutability;
        }
    }

    /// Rejects any `yield_!` inside `expr` (with `msg`) and any foreign
    /// macro whose tokens mention `yield_!`.
    fn check_no_yield(&mut self, expr: &syn::Expr, msg: &str) {
        let mut checker = YieldBan {
            msg,
            error: ErrorSink::default(),
        };
        checker.visit_expr(expr);
        if let Some(e) = checker.error.into_option() {
            self.err(e);
        }
    }
}

// === Error messages ===

const ERR_UNHOISTABLE: &str = "yield_! is not supported in this position: inside an \
     expression it is only supported when everything evaluated before it is a path, a \
     literal, or another yield_!, in an unconditionally evaluated position; bind the \
     resume value first: `let r = yield_!(..);` and use `r` here";
const ERR_TRAILING_YIELD: &str =
    "yield_! as the trailing expression is not supported; add a semicolon";
const ERR_FOREIGN_MACRO: &str = "yield_! cannot appear inside another macro invocation";
const ERR_LET_ELSE_INIT: &str = "yield_! in the initializer of `let ... else` is not supported";
const ERR_SIMPLE_BINDING: &str =
    "the binding of `let ... = yield_!(...)` must be a simple identifier";
const ERR_VALUE_LET_BINDING: &str =
    "the binding of a `let` whose initializer contains yield_! must be a simple identifier";
const ERR_YIELD_ARG: &str = "yield_! takes a single expression";
const ERR_VALUE_POSITION: &str = "yield_! in value position is only supported when \
     everything evaluated before it is a path, a literal, or another yield_!, or when \
     the value is the whole `let` initializer or function tail expression and is an \
     `if`/`match`/`loop`/block expression whose arms produce the value; bind the yield \
     first (`let r = yield_!(..);`) or, around control flow, declare \
     `let mut x: Option<T> = None;` before it, assign `x = Some(...);` where the value \
     is produced, and use `x.unwrap()` afterwards";
const ERR_TAIL: &str = "yield_! in the trailing expression is not supported here; \
     add a semicolon";
const ERR_COND: &str = "yield_! in a condition is only supported in `if` when \
     everything evaluated before it is a path, a literal, or another yield_!; bind it \
     first: `let c = yield_!(..);`. A `while` condition is re-evaluated every iteration \
     and cannot contain yield_!; restructure into `loop` with `if .. { break; }`";
const ERR_SCRUTINEE: &str = "yield_! in a scrutinee is only supported for \
     `match`/`if let`/`let ... else` when everything evaluated before it is a path, a \
     literal, or another yield_!; bind it first: `let s = yield_!(..);`. A `while let` \
     scrutinee is re-evaluated every iteration and cannot contain yield_!";
const ERR_GUARD: &str = "yield_! in a match guard is not supported";
const ERR_UNSAFE: &str = "yield_! inside an unsafe block is not supported";
const ERR_FOR_HEAD: &str = "yield_! in a `for` loop's iterator expression is only \
     supported when everything evaluated before it is a path, a literal, or another \
     yield_!; bind it first: `let r = yield_!(..);`";
const ERR_BREAK_VALUE: &str = "`break` with a value can only target a loop containing \
     yield_! when the loop is a `let` initializer or the function's trailing expression";
const ERR_YIELD_ALL_OPERAND: &str = "yield_all! takes a single variable holding the \
     coroutine to delegate to; bind it first: `let sub: SubTy = make_sub(..);` and then \
     `yield_all!(sub)`";
const ERR_YIELD_ALL_POSITION: &str = "yield_all! is only supported in statement \
     position (`yield_all!(sub);`), as a whole `let` initializer, or as the function's \
     trailing expression";

fn for_local_borrow_err(name: &syn::Ident) -> String {
    format!(
        "cannot iterate over a borrow of the local variable `{name}`: the iterator would be \
         stored in the coroutine state alongside `{name}` itself, making the state \
         self-referential; iterate by value instead"
    )
}

// === Statement lowering ===

impl Lowerer {
    /// Lowers a statement list. In `Discard` context the caller
    /// terminates the current block afterwards; in `FnReturn` context
    /// this terminates it with `Return`; in `Store` context every path
    /// ends by assigning the block's value (the trailing expression, or
    /// `()` without one) to the destination binding.
    fn lower_stmt_list(&mut self, stmts: &[syn::Stmt], ctx: TailCtx) {
        let n = stmts.len();
        let mut has_value_tail = false;
        for (i, stmt) in stmts.iter().enumerate() {
            match stmt {
                syn::Stmt::Expr(e, None) if i + 1 == n => {
                    has_value_tail = true;
                    self.lower_tail_expr(stmt, e, ctx);
                }
                // A brace-delimited trailing macro (`yield_all! { g }`)
                // parses as `Stmt::Macro` without a semicolon; paren and
                // bracket invocations parse as a trailing expression
                // (`Stmt::Expr(Expr::Macro, None)`) and take the arm
                // above. `yield_all!` is the one macro that produces a
                // value here.
                syn::Stmt::Macro(sm)
                    if i + 1 == n && sm.semi_token.is_none() && is_yield_all_macro(&sm.mac) =>
                {
                    has_value_tail = true;
                    self.lower_yield_all(&sm.mac, ctx);
                }
                _ => self.lower_stmt(stmt),
            }
        }
        match ctx {
            TailCtx::FnReturn => {
                if !self.is_current_terminated() {
                    self.terminate(Terminator::Return(syn::parse_quote!(())));
                }
            }
            // A block without a trailing expression evaluates to `()`.
            TailCtx::Store(id) if !has_value_tail && !self.is_current_terminated() => {
                let span = stmts
                    .last()
                    .map_or_else(proc_macro2::Span::call_site, |s| s.span());
                self.push_store(id, &unit_expr(span));
            }
            _ => {}
        }
    }

    fn lower_tail_expr(&mut self, stmt: &syn::Stmt, e: &syn::Expr, ctx: TailCtx) {
        // A trailing `break`/`continue` transfers control like any other.
        if matches!(e, syn::Expr::Break(_) | syn::Expr::Continue(_)) {
            return self.lower_stmt(stmt);
        }
        match ctx {
            TailCtx::FnReturn | TailCtx::Store(_) if contains_yield_expr(e) => {
                self.lower_value_expr(e, ctx);
            }
            TailCtx::FnReturn => {
                self.check_no_yield(e, ERR_TAIL); // foreign macro scan
                self.record_expr_uses(e, self.current);
                self.terminate(Terminator::Return(e.clone()));
            }
            TailCtx::Store(id) => self.push_store(id, e),
            TailCtx::Discard => {
                // A trailing loop always evaluates to `()` (a `break`
                // with a value cannot target a `Plain` frame), so it is
                // safe to lower as a statement even as the tail
                // expression.
                if is_block_like(e) || contains_yield_expr(e) {
                    self.lower_stmt(stmt);
                } else {
                    let wrapped = stmt_from_discarded_expr(e.clone());
                    self.push_opaque(&wrapped);
                }
            }
        }
    }

    /// Lowers a yield-containing expression whose value is needed (a
    /// `let` initializer or the function's trailing expression):
    /// control-flow expressions distribute the destination into their
    /// arms; anything else cannot suspend and is an error. Parentheses
    /// around the expression are transparent.
    fn lower_value_expr(&mut self, e: &syn::Expr, ctx: TailCtx) {
        match strip_parens(e) {
            syn::Expr::If(ei) => self.lower_if(ei, ctx),
            syn::Expr::Match(em) => self.lower_match(em, ctx),
            syn::Expr::Loop(el) => self.lower_loop(el, ctx),
            syn::Expr::Block(eb) => self.lower_block_stmt(eb, ctx),
            syn::Expr::Macro(em) if is_yield_all_macro(&em.mac) => {
                self.lower_yield_all(&em.mac, ctx);
            }
            // `while` and `for` loops always evaluate to `()`: lower as
            // a statement, then produce `()` at the exit.
            syn::Expr::While(_) | syn::Expr::ForLoop(_) => {
                self.lower_control_expr(e);
                if let TailCtx::Store(id) = ctx {
                    self.push_store(id, &unit_expr(e.span()));
                }
            }
            syn::Expr::Unsafe(eu) => {
                self.err(syn::Error::new_spanned(eu.unsafe_token, ERR_UNSAFE));
            }
            other => match ctx {
                TailCtx::FnReturn => {
                    // A yield inside the return value is a value-position
                    // yield. Lower the expression as a statement anyway
                    // to surface the most specific error; if it lowers
                    // cleanly the only problem is its tail position.
                    let before = self.errors.len();
                    self.lower_stmt(&syn::Stmt::Expr(other.clone(), None));
                    if self.errors.len() == before {
                        self.err(syn::Error::new_spanned(other, ERR_TAIL));
                    }
                }
                _ => self.err(syn::Error::new_spanned(other, ERR_VALUE_POSITION)),
            },
        }
    }

    /// Appends `let [mut] x[: T] = value;` assigning a block's value to
    /// the destination binding of a `Store` context, and records the
    /// definition. The binding enters the visible scope only after the
    /// whole initializer is lowered, so `value` still resolves against
    /// the enclosing environment.
    fn push_store(&mut self, id: BindingId, value: &syn::Expr) {
        self.check_no_yield(value, ERR_VALUE_POSITION);
        self.record_expr_uses(value, self.current);
        let b = &self.bindings[id.0];
        let ident = b.ident.clone();
        let mutability = b.mutability;
        let annotation = match &b.ty {
            TySource::Known(t) => Some(t.clone()),
            _ => None,
        };
        let span = value.span();
        let stmt: syn::Stmt = match annotation {
            Some(ty) => syn::parse_quote_spanned!(span => let #mutability #ident: #ty = #value;),
            None => syn::parse_quote_spanned!(span => let #mutability #ident = #value;),
        };
        self.blocks[self.current].stmts.push(stmt);
        self.blocks[self.current].defs.insert(id);
    }

    fn lower_stmt(&mut self, stmt: &syn::Stmt) {
        match stmt {
            // `yield_!(expr);`
            syn::Stmt::Macro(sm) if is_yield_macro(&sm.mac) => {
                if sm.semi_token.is_none() {
                    return self.err(syn::Error::new_spanned(sm, ERR_TRAILING_YIELD));
                }
                self.lower_yield(&sm.mac, ResumeTarget::Discard);
            }
            // `yield_all!(g);` in statement position (a trailing
            // `yield_all!(g)` is routed by `lower_stmt_list` instead).
            syn::Stmt::Macro(sm) if is_yield_all_macro(&sm.mac) => {
                self.lower_yield_all(&sm.mac, TailCtx::Discard);
            }
            // The same, reconstructed as an expression statement (arm
            // bodies are wrapped this way).
            syn::Stmt::Expr(syn::Expr::Macro(em), semi) if is_yield_macro(&em.mac) => {
                if semi.is_none() {
                    return self.err(syn::Error::new_spanned(em, ERR_TRAILING_YIELD));
                }
                self.lower_yield(&em.mac, ResumeTarget::Discard);
            }
            syn::Stmt::Expr(syn::Expr::Macro(em), _) if is_yield_all_macro(&em.mac) => {
                self.lower_yield_all(&em.mac, TailCtx::Discard);
            }
            // `let r = yield_!(expr);`
            syn::Stmt::Local(local)
                if matches!(
                    local.init.as_ref().map(|i| &*i.expr),
                    Some(syn::Expr::Macro(m)) if is_yield_macro(&m.mac)
                ) =>
            {
                self.lower_let_yield(local);
            }
            // `let x: T = yield_all!(g);`
            syn::Stmt::Local(local)
                if matches!(
                    local.init.as_ref().map(|i| &*i.expr),
                    Some(syn::Expr::Macro(m)) if is_yield_all_macro(&m.mac)
                ) =>
            {
                self.lower_let_yield_all(local);
            }
            // `__rv{k} = yield_!(__y{k});` — only synthesized inside a
            // yield_all! expansion, never accepted from user code.
            syn::Stmt::Expr(syn::Expr::Assign(assign), _)
                if self.yield_all_depth > 0
                    && matches!(&*assign.right, syn::Expr::Macro(m) if is_yield_macro(&m.mac)) =>
            {
                self.lower_rebind_yield(assign);
            }
            syn::Stmt::Expr(syn::Expr::Break(b), _) => self.lower_break(b),
            syn::Stmt::Expr(syn::Expr::Continue(c), _) => self.lower_continue(c),
            _ if !contains_yield_stmt(stmt) => self.push_opaque(stmt),
            // From here on the statement contains a yield_! somewhere.
            syn::Stmt::Local(local) if matches!(&local.init, Some(init) if init.diverge.is_some()) =>
            {
                self.lower_let_else(local);
            }
            syn::Stmt::Local(local) => self.lower_let_value(local),
            syn::Stmt::Expr(e, _) => self.lower_control_expr(e),
            // Unreachable in practice: only `Stmt::Macro`/`Stmt::Item`
            // remain here, and neither can contain a yield by this
            // point — `yield_!`/`yield_all!` statement macros were
            // handled above, a foreign macro's tokens don't count as
            // yields, and items are separate scopes skipped by the yield
            // scan — so both fall into the no-yield arm instead. Kept
            // for match exhaustiveness, with opaque as a safe fallback.
            _ => self.push_opaque(stmt),
        }
    }

    fn lower_let_yield(&mut self, local: &syn::Local) {
        let init = local.init.as_ref().expect("BUG: checked by caller");
        if init.diverge.is_some() {
            return self.err(syn::Error::new_spanned(&init.expr, ERR_LET_ELSE_INIT));
        }
        let syn::Expr::Macro(m) = &*init.expr else {
            unreachable!("BUG: checked by the caller")
        };
        let (pat, ty) = match &local.pat {
            syn::Pat::Type(pt) => (&*pt.pat, Some((*pt.ty).clone())),
            other => (other, None),
        };
        let binding = match pat {
            syn::Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => {
                Box::new(ResumeBindingSpec {
                    ident: pi.ident.clone(),
                    mutability: pi.mutability,
                    ty,
                })
            }
            other => {
                return self.err(syn::Error::new_spanned(other, ERR_SIMPLE_BINDING));
            }
        };
        self.lower_yield(&m.mac, ResumeTarget::Bind(binding));
    }

    /// `let x[: T] = <control-flow expr containing yield_!>;` — the
    /// let-initializer form of value-position yield. The binding becomes
    /// a "block argument" of the join block: every arm of the initializer
    /// ends by assigning its value to the binding (`push_store`), so the
    /// ordinary liveness machinery carries it into the join state. The
    /// join is a multi-predecessor block and thus always a state variant,
    /// so the binding needs a syntactically known type; without the
    /// annotation the analysis reports the usual annotate-the-type error.
    fn lower_let_value(&mut self, local: &syn::Local) {
        // The statement contains a yield_! but has no initializer, so it
        // sits in the binding pattern or the type annotation (e.g.
        // `let yield_!(x);`) — neither is a supported position.
        let Some(init) = local.init.as_ref() else {
            return self.err(syn::Error::new_spanned(local, ERR_UNHOISTABLE));
        };
        let expr = strip_parens(&init.expr);
        let supported = matches!(
            expr,
            syn::Expr::If(_)
                | syn::Expr::Match(_)
                | syn::Expr::Loop(_)
                | syn::Expr::While(_)
                | syn::Expr::ForLoop(_)
                | syn::Expr::Block(_)
                | syn::Expr::Unsafe(_)
        ) || matches!(expr, syn::Expr::Macro(m) if is_yield_all_macro(&m.mac));
        if !supported {
            return self.err(syn::Error::new_spanned(local, ERR_VALUE_POSITION));
        }
        self.lower_value_let(local, |lw, ctx| lw.lower_value_expr(expr, ctx));
    }

    /// Sets up the destination of a `let` whose initializer is lowered
    /// into CFG structure (a value-position yield or `yield_all!`) and
    /// runs `lower` with the resulting context: `_` discards the value
    /// and any other non-identifier pattern is an error.
    fn lower_value_let(&mut self, local: &syn::Local, lower: impl FnOnce(&mut Self, TailCtx)) {
        let (pat, annotation) = match &local.pat {
            syn::Pat::Type(pt) => (&*pt.pat, Some((*pt.ty).clone())),
            other => (other, None),
        };
        // `let _ = <init>;` binds nothing: the arm values are simply
        // discarded, like a statement-position construct.
        if matches!(pat, syn::Pat::Wild(_)) {
            return lower(self, TailCtx::Discard);
        }
        let syn::Pat::Ident(pi) = pat else {
            return self.err(syn::Error::new_spanned(pat, ERR_VALUE_LET_BINDING));
        };
        if pi.by_ref.is_some() || pi.subpat.is_some() {
            return self.err(syn::Error::new_spanned(pi, ERR_VALUE_LET_BINDING));
        }
        // The binding is created up front (the arms assign to it) but
        // enters the visible scope only after the initializer is fully
        // lowered: the initializer sees the enclosing environment, as in
        // any `let`.
        let id = BindingId(self.bindings.len());
        self.bindings.push(Binding {
            ident: pi.ident.clone(),
            mutability: pi.mutability,
            kind: BindingKind::Local,
            ty: annotation
                .clone()
                .map_or(TySource::Unknown, TySource::Known),
            borrow: self.classify_borrow(None, annotation.as_ref()),
            def_stmt: None,
        });
        lower(self, TailCtx::Store(id));
        self.scopes
            .last_mut()
            .expect("BUG: no scope")
            .insert(pi.ident.to_string(), id);
    }

    /// `let pat = scrutinee else { .. };` containing a yield_! (rustc
    /// requires the `else` block to diverge, so the yield is inside it):
    /// desugared into a refutable two-arm `match` whose `_` arm is the
    /// diverging block and whose pattern arm is the continuation of the
    /// enclosing statement list. The pattern's bindings enter the
    /// enclosing scope as arm-pattern bindings, so the usual
    /// rebind-before-yield constraint applies to them.
    fn lower_let_else(&mut self, local: &syn::Local) {
        let init = local.init.as_ref().expect("BUG: checked by caller");
        let (else_token, else_expr) = init.diverge.as_ref().expect("BUG: checked by caller");
        let syn::Expr::Block(else_block) = &**else_expr else {
            unreachable!("a `let ... else` body is always a block")
        };
        self.check_no_yield(&init.expr, ERR_LET_ELSE_INIT);
        self.record_expr_uses(&init.expr, self.current);
        let (pat, annotation) = match &local.pat {
            syn::Pat::Type(pt) => (&*pt.pat, Some(&*pt.ty)),
            other => (other, None),
        };
        // A `let pat: T = ..` ascription has no place on a match arm
        // pattern; reapply it to the scrutinee instead.
        let scrutinee: syn::Expr = match annotation {
            Some(ty) => {
                let e = &init.expr;
                syn::parse_quote_spanned!(e.span() => { let __scrutinee: #ty = #e; __scrutinee })
            }
            None => (*init.expr).clone(),
        };
        let cont = self.new_block(false);
        let else_bb = self.new_block(false);
        self.terminate(refutable_match(&scrutinee, pat, cont, else_bb));
        self.current = else_bb;
        self.in_scope(|lw| lw.lower_stmt_list(&else_block.block.stmts, TailCtx::Discard));
        // In valid code the `else` block diverges, so a fall-through
        // terminator is unreachable; it only has to typecheck (`return`
        // is rewritten into an opaque statement before lowering and a
        // trailing `panic!()` stays opaque, so the block can still look
        // open here). Invalid, non-diverging code reaches the
        // `unreachable!` at run time instead of failing to compile.
        if !self.is_current_terminated() {
            self.terminate(Terminator::Unreachable(syn::parse_quote_spanned!(
                else_token.span() =>
                    ::core::unreachable!("the `else` block of `let ... else` must diverge")
            )));
        }
        self.current = cont;
        self.define_pat_bindings(pat, cont, BindingKind::ArmPat, None);
    }

    /// Ends the current block with a `Yield` terminator and switches to
    /// the resume-point continuation block.
    fn lower_yield(&mut self, mac: &syn::Macro, target: ResumeTarget) {
        let value: syn::Expr = if mac.tokens.is_empty() {
            syn::parse_quote!(())
        } else {
            match mac.parse_body() {
                Ok(e) => e,
                Err(_) => {
                    self.err(syn::Error::new_spanned(&mac.tokens, ERR_YIELD_ARG));
                    syn::parse_quote!(())
                }
            }
        };
        // The yielded value is evaluated before the transition: its uses
        // belong to the yielding block, and it must not itself suspend.
        self.check_no_yield(&value, ERR_UNHOISTABLE);
        self.record_expr_uses(&value, self.current);
        let next = self.new_block(true);
        let resume_binding = match target {
            ResumeTarget::Discard => None,
            ResumeTarget::Bind(spec) => {
                let id = self.define(&spec.ident, next, BindingKind::Resume, None);
                let b = &mut self.bindings[id.0];
                b.mutability = spec.mutability;
                if let Some(t) = &spec.ty {
                    b.ty = TySource::Known(t.clone());
                }
                Some(ResumeBinding {
                    binding: id,
                    mutability: spec.mutability,
                    ty: spec.ty,
                })
            }
            // The existing binding is re-defined (not read) by the
            // resume transition, exactly like a fresh resume binding.
            ResumeTarget::Rebind(id) => {
                self.blocks[next].defs.insert(id);
                Some(ResumeBinding {
                    binding: id,
                    mutability: None,
                    ty: None,
                })
            }
        };
        self.terminate(Terminator::Yield {
            value,
            resume_binding,
            next,
        });
        self.current = next;
    }

    /// `yield_all!(g)` — delegates to the coroutine held by the variable
    /// `g`: each inner yield is forwarded to the caller, each resume
    /// value is forwarded back in, and the inner completion value is the
    /// expansion's value. Desugared into source that the ordinary
    /// machinery lowers:
    ///
    /// ```text
    /// let mut __dg0 = g;
    /// match ::diapause::Coroutine::start(&mut __dg0) {
    ///     Complete(__v0) => __v0,
    ///     Yielded(__y0) => {
    ///         let __rv0 = yield_!(__y0);
    ///         loop {
    ///             match ::diapause::Coroutine::resume(&mut __dg0, __rv0) {
    ///                 Complete(__v0) => break __v0,
    ///                 Yielded(__y0) => {
    ///                     __rv0 = yield_!(__y0); // internal rebind form
    ///                 }
    ///             }
    ///         }
    ///     }
    /// }
    /// ```
    ///
    /// Only `__dg{k}` and `__rv{k}` are live at the delegation loop's
    /// header: the coroutine's type follows the operand variable (the
    /// usual known-variable move rule — hence the variable-only operand
    /// restriction) and the resume value's type defaults to the outer
    /// resume type like any resume binding. Everything else (`__y{k}`,
    /// `__v{k}`) is consumed before the next transition. A yield/resume
    /// type mismatch between the inner and outer coroutine surfaces as
    /// an ordinary type error in the generated code.
    fn lower_yield_all(&mut self, mac: &syn::Macro, ctx: TailCtx) {
        let operand: Option<syn::Expr> = if mac.tokens.is_empty() {
            None
        } else {
            mac.parse_body().ok()
        };
        let Some(operand) = operand else {
            return self.err(syn::Error::new(mac.span(), ERR_YIELD_ALL_OPERAND));
        };
        let inner = strip_parens(&operand);
        if !matches!(inner, syn::Expr::Path(p) if p.qself.is_none() && p.path.get_ident().is_some())
        {
            return self.err(syn::Error::new_spanned(inner, ERR_YIELD_ALL_OPERAND));
        }
        let span = inner.span();
        let k = self.yield_all_count;
        self.yield_all_count += 1;
        let dg = syn::Ident::new(&format!("__dg{k}"), span);
        let y = syn::Ident::new(&format!("__y{k}"), span);
        let rv = syn::Ident::new(&format!("__rv{k}"), span);
        let v = syn::Ident::new(&format!("__v{k}"), span);

        // In `Discard` context the completion values are dropped and the
        // loop breaks without a value (a valued `break` may not target a
        // statement-position loop).
        let em: syn::ExprMatch = if matches!(ctx, TailCtx::Discard) {
            syn::parse_quote_spanned!(span => match ::diapause::Coroutine::start(&mut #dg) {
                ::diapause::CoroutineState::Complete(_) => {}
                ::diapause::CoroutineState::Yielded(#y) => {
                    let #rv = yield_!(#y);
                    loop {
                        match ::diapause::Coroutine::resume(&mut #dg, #rv) {
                            ::diapause::CoroutineState::Complete(_) => break,
                            ::diapause::CoroutineState::Yielded(#y) => {
                                #rv = yield_!(#y);
                            }
                        }
                    }
                }
            })
        } else {
            syn::parse_quote_spanned!(span => match ::diapause::Coroutine::start(&mut #dg) {
                ::diapause::CoroutineState::Complete(#v) => #v,
                ::diapause::CoroutineState::Yielded(#y) => {
                    let #rv = yield_!(#y);
                    loop {
                        match ::diapause::Coroutine::resume(&mut #dg, #rv) {
                            ::diapause::CoroutineState::Complete(#v) => break #v,
                            ::diapause::CoroutineState::Yielded(#y) => {
                                #rv = yield_!(#y);
                            }
                        }
                    }
                }
            })
        };

        // The synthetic bindings get their own scope so the enclosing
        // code never sees them.
        self.in_scope(|lw| {
            // `let mut __dg{k} = <operand>;` — the coroutine moves into
            // the state under a fresh name; its type follows the operand.
            let ty = lw.infer_ty_source(inner);
            let stmt: syn::Stmt = syn::parse_quote_spanned!(span => let mut #dg = #inner;);
            lw.record_stmt_uses(&stmt);
            let stmt_idx = lw.blocks[lw.current].stmts.len();
            let id = lw.define(&dg, lw.current, BindingKind::Delegate, Some(stmt_idx));
            let b = &mut lw.bindings[id.0];
            b.mutability = Some(syn::Token![mut](span));
            b.ty = ty;
            lw.blocks[lw.current].stmts.push(stmt);

            lw.yield_all_depth += 1;
            lw.lower_match(&em, ctx);
            lw.yield_all_depth -= 1;
        });
    }

    /// `let x: T = yield_all!(g);` — the let-initializer form. The value
    /// crosses the join after the delegation loop into a state variant,
    /// so the annotation is required by the usual value-`let` rule.
    fn lower_let_yield_all(&mut self, local: &syn::Local) {
        let init = local.init.as_ref().expect("BUG: checked by caller");
        if init.diverge.is_some() {
            return self.err(syn::Error::new_spanned(&init.expr, ERR_LET_ELSE_INIT));
        }
        let syn::Expr::Macro(m) = &*init.expr else {
            unreachable!("BUG: checked by the caller")
        };
        self.lower_value_let(local, |lw, ctx| lw.lower_yield_all(&m.mac, ctx));
    }

    /// The internal `__rv{k} = yield_!(__y{k});` form synthesized by
    /// `lower_yield_all` (and gated by `yield_all_depth`): a yield whose
    /// resume value re-defines the existing `__rv{k}` binding.
    fn lower_rebind_yield(&mut self, assign: &syn::ExprAssign) {
        let syn::Expr::Macro(m) = &*assign.right else {
            unreachable!("BUG: checked by the caller")
        };
        let syn::Expr::Path(p) = &*assign.left else {
            unreachable!("BUG: the rebind target is a synthesized identifier")
        };
        let ident = p.path.get_ident().expect("BUG: synthesized identifier");
        let id = self
            .resolve(&ident.to_string())
            .expect("BUG: rebind target not in scope");
        self.lower_yield(&m.mac, ResumeTarget::Rebind(id));
    }

    fn lower_break(&mut self, b: &syn::ExprBreak) {
        let frame = match &b.label {
            Some(l) => self.find_labeled_frame(&l.ident.to_string()),
            None => self.innermost_loop_frame(),
        };
        let Some(frame) = frame else {
            return self.err(syn::Error::new_spanned(
                b,
                match &b.label {
                    Some(l) => format!("use of undeclared label `{l}`"),
                    None => "`break` outside of a loop".to_string(),
                },
            ));
        };
        let dest = frame.dest;
        // A `break` without a value produces `()` where a value frame
        // expects one; the type mismatch, if any, is rustc's to report.
        let value = |b: &syn::ExprBreak| -> syn::Expr {
            b.expr
                .as_deref()
                .cloned()
                .unwrap_or_else(|| unit_expr(b.break_token.span()))
        };
        match dest {
            BreakDest::Plain(exit) => {
                if let Some(v) = &b.expr {
                    self.err(syn::Error::new_spanned(v, ERR_BREAK_VALUE));
                }
                self.terminate(Terminator::Goto(exit));
            }
            BreakDest::Store { binding, exit } => {
                self.push_store(binding, &value(b));
                self.terminate(Terminator::Goto(exit));
            }
            BreakDest::Return => {
                let v = value(b);
                self.check_no_yield(&v, ERR_VALUE_POSITION);
                self.record_expr_uses(&v, self.current);
                self.terminate(Terminator::Return(v));
            }
        }
        // Anything after the jump is unreachable; lower it into a fresh
        // block that simplification will drop.
        self.current = self.new_block(false);
    }

    fn lower_continue(&mut self, c: &syn::ExprContinue) {
        let frame = match &c.label {
            Some(l) => self.find_labeled_frame(&l.ident.to_string()),
            None => self.innermost_loop_frame(),
        };
        let Some(frame) = frame else {
            return self.err(syn::Error::new_spanned(
                c,
                match &c.label {
                    Some(l) => format!("use of undeclared label `{l}`"),
                    None => "`continue` outside of a loop".to_string(),
                },
            ));
        };
        let Some(header) = frame.header else {
            return self.err(syn::Error::new_spanned(
                c,
                "`continue` cannot target a labeled block",
            ));
        };
        self.terminate(Terminator::Goto(header));
        self.current = self.new_block(false);
    }

    fn find_labeled_frame(&self, name: &str) -> Option<&Frame> {
        self.labels
            .iter()
            .rev()
            .find(|f| f.label.as_deref() == Some(name))
    }

    fn innermost_loop_frame(&self) -> Option<&Frame> {
        self.labels.iter().rev().find(|f| f.header.is_some())
    }

    /// Appends a statement without yield_! to the current block, after
    /// validating it and rewriting any `break`/`continue` targeting an
    /// expanded loop or labeled block into a jump marker.
    fn push_opaque(&mut self, stmt: &syn::Stmt) {
        let stmt = self.rewrite_opaque(stmt);
        self.record_stmt_uses(&stmt);
        if let syn::Stmt::Local(local) = &stmt {
            // The statement is not pushed yet, so its future index in
            // `stmts` is the current length.
            let stmt_idx = self.blocks[self.current].stmts.len();
            self.define_local(local, stmt_idx);
        }
        self.blocks[self.current].stmts.push(stmt);
    }

    /// Introduces the bindings of an opaque `let`, classifying the type
    /// source and borrow kind of a simple-identifier binding.
    /// Classification runs before the bindings enter scope, so the
    /// initializer resolves against the enclosing environment
    /// (`let x = x;` sees the outer `x`). `stmt_idx` is this `let`'s
    /// index within the current block's `stmts`.
    fn define_local(&mut self, local: &syn::Local, stmt_idx: usize) {
        let init = local.init.as_ref().map(|i| &*i.expr);
        let (pat, annotation) = match &local.pat {
            syn::Pat::Type(pt) => (&*pt.pat, Some(&*pt.ty)),
            other => (other, None),
        };
        match pat {
            syn::Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => {
                let ty = match annotation {
                    Some(t) => TySource::Known(t.clone()),
                    None => init.map_or(TySource::Unknown, |e| self.infer_ty_source(e)),
                };
                let borrow = self.classify_borrow(init, annotation);
                let id = self.define(&pi.ident, self.current, BindingKind::Local, Some(stmt_idx));
                let b = &mut self.bindings[id.0];
                b.mutability = pi.mutability;
                b.ty = ty;
                b.borrow = borrow;
            }
            // Destructuring patterns: every bound identifier becomes a
            // binding of unknown type.
            other => {
                self.define_pat_bindings(other, self.current, BindingKind::Local, Some(stmt_idx))
            }
        }
    }

    /// Pushes the synthesized `let <pat> = <source>;` that destructures
    /// a pattern argument at the top of the entry block. Its bindings
    /// get `BindingKind::ArgPat`. Argument bindings share one namespace
    /// (as in a plain fn, where duplicates are E0415), and the starter
    /// fn no longer carries the original patterns for rustc to check,
    /// so name clashes are rejected here.
    fn push_arg_pat_let(&mut self, pat: &syn::Pat, source: &syn::Ident) {
        let stmt: syn::Stmt = syn::parse_quote!(let #pat = #source;);
        self.record_stmt_uses(&stmt);
        let stmt_idx = self.blocks[self.current].stmts.len();
        let mut c = PatBindingCollector::default();
        c.visit_pat(pat);
        for (ident, mutability) in c.bindings {
            if self.resolve(&ident.to_string()).is_some() {
                self.err(syn::Error::new(
                    ident.span(),
                    format!("identifier `{ident}` is bound more than once in the argument list"),
                ));
                continue;
            }
            let id = self.define(&ident, self.current, BindingKind::ArgPat, Some(stmt_idx));
            self.bindings[id.0].mutability = mutability;
        }
        self.blocks[self.current].stmts.push(stmt);
    }
}

fn is_block_like(e: &syn::Expr) -> bool {
    matches!(
        e,
        syn::Expr::If(_)
            | syn::Expr::Match(_)
            | syn::Expr::Block(_)
            | syn::Expr::Unsafe(_)
            | syn::Expr::Loop(_)
            | syn::Expr::While(_)
            | syn::Expr::ForLoop(_)
            | syn::Expr::TryBlock(_)
            | syn::Expr::Const(_)
    )
}

/// A `()` expression carrying `span`, used for the implicit value of
/// valueless paths in `Store`/`FnReturn` contexts (`break;`, a missing
/// `else`, a block without a trailing expression, `while`/`for` exits).
fn unit_expr(span: proc_macro2::Span) -> syn::Expr {
    syn::parse_quote_spanned!(span => ())
}

/// A two-arm `Match` terminator testing `pat` against `scrutinee`: the
/// shape shared by `while let`, `if let`, and `let ... else`.
fn refutable_match(
    scrutinee: &syn::Expr,
    pat: &syn::Pat,
    matched: BlockId,
    unmatched: BlockId,
) -> Terminator {
    Terminator::Match {
        scrutinee: scrutinee.clone(),
        arms: vec![
            MatchArm {
                pat: pat.clone(),
                guard: None,
                body: matched,
            },
            MatchArm {
                pat: syn::parse_quote!(_),
                guard: None,
                body: unmatched,
            },
        ],
    }
}

/// Whether an `if`/`while` condition is an edition-2024 let chain:
/// `let` patterns combined with `&&`. A lone `if let`/`while let`
/// condition is a plain `Expr::Let`, not a chain.
pub(crate) fn is_let_chain(cond: &syn::Expr) -> bool {
    fn contains_let(e: &syn::Expr) -> bool {
        match e {
            syn::Expr::Let(_) => true,
            syn::Expr::Binary(b) if matches!(b.op, syn::BinOp::And(_)) => {
                contains_let(&b.left) || contains_let(&b.right)
            }
            _ => false,
        }
    }
    !matches!(cond, syn::Expr::Let(_)) && contains_let(cond)
}

/// One link of an edition-2024 let-chain condition, in left-to-right
/// evaluation order.
enum LetChainLink<'a> {
    Cond(&'a syn::Expr),
    Let(&'a syn::ExprLet),
}

/// Splits a let-chain condition into its `&&`-joined links, left to
/// right (the same recursion `is_let_chain` uses to detect one). Only
/// meaningful when [`is_let_chain`] holds; a chain always has at least
/// two links, since a lone `let` is not one.
fn flatten_let_chain(cond: &syn::Expr) -> Vec<LetChainLink<'_>> {
    fn go<'a>(e: &'a syn::Expr, out: &mut Vec<LetChainLink<'a>>) {
        match e {
            syn::Expr::Binary(b) if matches!(b.op, syn::BinOp::And(_)) => {
                go(&b.left, out);
                go(&b.right, out);
            }
            syn::Expr::Let(el) => out.push(LetChainLink::Let(el)),
            other => out.push(LetChainLink::Cond(other)),
        }
    }
    let mut out = Vec::new();
    go(cond, &mut out);
    out
}

/// Turns a discarded expression into a statement: block-like expressions
/// (if/match/loop/etc.) are already valid statements on their own, while
/// everything else needs a semicolon to keep the statement sequence valid.
fn stmt_from_discarded_expr(expr: syn::Expr) -> syn::Stmt {
    let semi = if is_block_like(&expr) {
        None
    } else {
        Some(Default::default())
    };
    syn::Stmt::Expr(expr, semi)
}

// === Control-flow expansion (statements that contain yield_!) ===

impl Lowerer {
    fn lower_control_expr(&mut self, e: &syn::Expr) {
        match strip_parens(e) {
            syn::Expr::If(ei) => self.lower_if(ei, TailCtx::Discard),
            syn::Expr::Match(em) => self.lower_match(em, TailCtx::Discard),
            syn::Expr::Loop(el) => self.lower_loop(el, TailCtx::Discard),
            syn::Expr::While(ew) => self.lower_while(ew),
            syn::Expr::ForLoop(ef) => self.lower_for(ef),
            syn::Expr::Block(eb) => self.lower_block_stmt(eb, TailCtx::Discard),
            // Reached only parenthesized: a bare `yield_all!(..);`
            // statement is routed by `lower_stmt` before it gets here.
            syn::Expr::Macro(em) if is_yield_all_macro(&em.mac) => {
                self.lower_yield_all(&em.mac, TailCtx::Discard);
            }
            syn::Expr::Unsafe(eu) => {
                self.err(syn::Error::new_spanned(eu.unsafe_token, ERR_UNSAFE));
            }
            // Point at each yield_! (or yield_all!/foreign macro) in
            // the statement rather than the whole expression.
            other => self.check_no_yield(other, ERR_UNHOISTABLE),
        }
    }

    /// Terminates the current block with a jump to `target` unless a
    /// tail already terminated it (a `FnReturn` arm ends in `Return`).
    fn goto_if_open(&mut self, target: BlockId) {
        if !self.is_current_terminated() {
            self.terminate(Terminator::Goto(target));
        }
    }

    fn lower_if(&mut self, ei: &syn::ExprIf, ctx: TailCtx) {
        let join = self.new_block(false);
        self.lower_if_arms(ei, join, ctx);
        self.current = join;
    }

    /// Lowers one `if`/`else if` link of a chain; every arm exits to the
    /// shared `join` block. The arms' trailing expressions are handled
    /// per `ctx` (discarded, assigned to a binding, or returned).
    fn lower_if_arms(&mut self, ei: &syn::ExprIf, join: BlockId, ctx: TailCtx) {
        let then_bb = self.new_block(false);
        // Without an `else`, the false edge produces `()`: in `Store`
        // context it needs a block that assigns it (otherwise the
        // binding would look live at the function entry).
        let else_bb = match (&ei.else_branch, ctx) {
            (None, TailCtx::Store(_)) | (Some(_), _) => self.new_block(false),
            (None, _) => join,
        };
        if is_let_chain(&ei.cond) {
            // Every link's bindings must stay visible through later
            // links and the `then` branch, and nowhere else: one scope
            // covers wiring the whole chain and lowering the branch.
            self.in_scope(|lw| {
                lw.lower_let_chain_cond(&ei.cond, then_bb, else_bb);
                lw.lower_stmt_list(&ei.then_branch.stmts, ctx);
            });
        } else {
            // `if let pat = scrutinee` lowers like a two-arm `match`: the
            // pattern arm enters the then block, `_` the else block.
            let pat = match &*ei.cond {
                syn::Expr::Let(el) => {
                    self.check_no_yield(&el.expr, ERR_SCRUTINEE);
                    self.record_expr_uses(&el.expr, self.current);
                    self.terminate(refutable_match(&el.expr, &el.pat, then_bb, else_bb));
                    Some(&*el.pat)
                }
                cond => {
                    self.check_no_yield(cond, ERR_COND);
                    self.record_expr_uses(cond, self.current);
                    self.terminate(Terminator::Branch {
                        cond: cond.clone(),
                        then_: then_bb,
                        else_: else_bb,
                    });
                    None
                }
            };
            self.current = then_bb;
            self.in_scope(|lw| {
                if let Some(pat) = pat {
                    lw.define_pat_bindings(pat, then_bb, BindingKind::ArmPat, None);
                }
                lw.lower_stmt_list(&ei.then_branch.stmts, ctx);
            });
        }
        self.goto_if_open(join);
        match &ei.else_branch {
            Some((_, else_expr)) => {
                self.current = else_bb;
                match &**else_expr {
                    syn::Expr::Block(b) => {
                        self.in_scope(|lw| lw.lower_stmt_list(&b.block.stmts, ctx));
                        self.goto_if_open(join);
                    }
                    syn::Expr::If(nested) => self.lower_if_arms(nested, join, ctx),
                    _ => unreachable!("else branch is always a block or an if"),
                }
            }
            None => {
                if let TailCtx::Store(id) = ctx {
                    self.current = else_bb;
                    self.push_store(id, &unit_expr(ei.if_token.span()));
                    self.terminate(Terminator::Goto(join));
                }
            }
        }
    }

    /// Wires an edition-2024 let-chain condition (`c1 && let p = e &&
    /// c2 && ..`) into a chain of `Branch`/`Match` terminators: each
    /// link's failure jumps straight to `else_bb`, so the source-level
    /// `else` never gets duplicated into one copy per link. A `let`
    /// link's pattern bindings are defined in the block reached right
    /// after it succeeds, so later links and `then_bb` see them; the
    /// caller must run this inside the scope that should hold them
    /// (they must not outlive the `if`/`while`). Only the leading link
    /// can have been hoisted (see `hoist.rs`), so a `yield_!` surviving
    /// in a later link is rejected exactly as in a bare condition or
    /// scrutinee. Leaves `self.current` set to `then_bb`.
    fn lower_let_chain_cond(&mut self, cond: &syn::Expr, then_bb: BlockId, else_bb: BlockId) {
        let links = flatten_let_chain(cond);
        let last = links.len() - 1;
        for (i, link) in links.into_iter().enumerate() {
            let target = if i == last {
                then_bb
            } else {
                self.new_block(false)
            };
            match link {
                LetChainLink::Cond(c) => {
                    self.check_no_yield(c, ERR_COND);
                    self.record_expr_uses(c, self.current);
                    self.terminate(Terminator::Branch {
                        cond: c.clone(),
                        then_: target,
                        else_: else_bb,
                    });
                }
                LetChainLink::Let(el) => {
                    self.check_no_yield(&el.expr, ERR_SCRUTINEE);
                    self.record_expr_uses(&el.expr, self.current);
                    self.terminate(refutable_match(&el.expr, &el.pat, target, else_bb));
                    self.define_pat_bindings(&el.pat, target, BindingKind::ArmPat, None);
                }
            }
            self.current = target;
        }
    }

    fn lower_match(&mut self, em: &syn::ExprMatch, ctx: TailCtx) {
        self.check_no_yield(&em.expr, ERR_SCRUTINEE);
        self.record_expr_uses(&em.expr, self.current);
        let match_bb = self.current;
        let join = self.new_block(false);
        let mut arms = Vec::with_capacity(em.arms.len());
        for arm in &em.arms {
            let guard = arm.guard.as_ref().map(|(_, g)| (**g).clone());
            if let Some(g) = &guard {
                self.check_no_yield(g, ERR_GUARD);
                // Over-approximation: guard uses of the arm's own pattern
                // bindings resolve to same-named outer bindings, if any.
                self.record_expr_uses(g, match_bb);
            }
            let body_bb = self.new_block(false);
            self.current = body_bb;
            self.in_scope(|lw| {
                lw.define_pat_bindings(&arm.pat, body_bb, BindingKind::ArmPat, None);
                // In value contexts the arm body is the value; in
                // statement position it is discarded and needs the
                // trailing semicolon to stay a valid statement.
                let body_stmt = match ctx {
                    TailCtx::Discard => wrap_arm_body(&arm.body),
                    _ => syn::Stmt::Expr((*arm.body).clone(), None),
                };
                lw.lower_stmt_list(std::slice::from_ref(&body_stmt), ctx);
            });
            self.goto_if_open(join);
            arms.push(MatchArm {
                pat: arm.pat.clone(),
                guard,
                body: body_bb,
            });
        }
        self.set_terminator(
            match_bb,
            Terminator::Match {
                scrutinee: (*em.expr).clone(),
                arms,
            },
        );
        self.current = join;
    }

    /// Expands the skeleton shared by `loop`/`while`/`while let`/`for`:
    /// a header reached by an initial goto, a `break`/`continue` frame
    /// bound to it, and a backedge from the body to the header once
    /// it's lowered.
    ///
    /// `self.current` is the header when `setup` runs; `setup` creates
    /// (or reuses, for a plain `loop`) the `body` and `exit` blocks and
    /// installs the header's terminator (a `Branch`, `Match`, or
    /// `IterNext`), or leaves it unset for the body to fill in, as
    /// `loop` does. `bind_pat` runs with the loop's scope already
    /// pushed, before `body_stmts` are lowered, to bind a loop pattern
    /// (`while let`'s / `for`'s); it is a no-op for `loop`/`while`.
    fn with_loop_frame(
        &mut self,
        label: &Option<syn::Label>,
        body_stmts: &[syn::Stmt],
        ctx: TailCtx,
        setup: impl FnOnce(&mut Self, BlockId) -> (BlockId, BlockId),
        bind_pat: impl FnOnce(&mut Self, BlockId),
    ) {
        let header = self.new_block(false);
        self.terminate(Terminator::Goto(header));
        self.current = header;
        let (body, exit) = setup(self, header);
        self.labels.push(Frame {
            label: label.as_ref().map(|l| l.name.ident.to_string()),
            header: Some(header),
            dest: BreakDest::of(ctx, exit),
        });
        self.current = body;
        self.in_scope(|lw| {
            bind_pat(lw, body);
            lw.lower_stmt_list(body_stmts, TailCtx::Discard);
        });
        self.terminate(Terminator::Goto(header));
        self.labels.pop();
        self.current = exit;
    }

    /// In value contexts (`ctx` is `Store`/`FnReturn`) the loop's value
    /// comes exclusively from its `break`s, which the frame's `dest`
    /// routes; the exit block is then reachable only through them.
    fn lower_loop(&mut self, el: &syn::ExprLoop, ctx: TailCtx) {
        self.with_loop_frame(
            &el.label,
            &el.body.stmts,
            ctx,
            |lw, header| (header, lw.new_block(false)),
            |_, _| {},
        );
    }

    fn lower_while(&mut self, ew: &syn::ExprWhile) {
        if let syn::Expr::Let(el) = &*ew.cond {
            return self.lower_while_let(ew, el);
        }
        if is_let_chain(&ew.cond) {
            return self.lower_while_let_chain(ew);
        }
        self.with_loop_frame(
            &ew.label,
            &ew.body.stmts,
            TailCtx::Discard,
            |lw, header| {
                lw.check_no_yield(&ew.cond, ERR_COND);
                lw.record_expr_uses(&ew.cond, header);
                let body = lw.new_block(false);
                let exit = lw.new_block(false);
                lw.terminate(Terminator::Branch {
                    cond: (*ew.cond).clone(),
                    then_: body,
                    else_: exit,
                });
                (body, exit)
            },
            |_, _| {},
        );
    }

    fn lower_while_let(&mut self, ew: &syn::ExprWhile, el: &syn::ExprLet) {
        self.with_loop_frame(
            &ew.label,
            &ew.body.stmts,
            TailCtx::Discard,
            |lw, header| {
                lw.check_no_yield(&el.expr, ERR_SCRUTINEE);
                lw.record_expr_uses(&el.expr, header);
                let body = lw.new_block(false);
                let exit = lw.new_block(false);
                lw.set_terminator(header, refutable_match(&el.expr, &el.pat, body, exit));
                (body, exit)
            },
            |lw, body| lw.define_pat_bindings(&el.pat, body, BindingKind::ArmPat, None),
        );
    }

    /// `while c1 && let p = e && c2 { .. }` — an edition-2024 let-chain
    /// condition in a `while` loop: equivalent to `loop { if <chain> {
    /// .. } else { break; } }`. Unlike [`Lowerer::with_loop_frame`], the
    /// whole condition is wired from inside the loop's own scope rather
    /// than before it: a `let` link's bindings must be visible to later
    /// links and to the body, and must go out of scope again at the end
    /// of every iteration (the chain is re-evaluated from the top each
    /// time around, exactly like a `while let` scrutinee).
    fn lower_while_let_chain(&mut self, ew: &syn::ExprWhile) {
        let header = self.new_block(false);
        self.terminate(Terminator::Goto(header));
        let exit = self.new_block(false);
        self.labels.push(Frame {
            label: ew.label.as_ref().map(|l| l.name.ident.to_string()),
            header: Some(header),
            dest: BreakDest::of(TailCtx::Discard, exit),
        });
        self.current = header;
        self.in_scope(|lw| {
            let body = lw.new_block(false);
            lw.lower_let_chain_cond(&ew.cond, body, exit);
            lw.lower_stmt_list(&ew.body.stmts, TailCtx::Discard);
        });
        self.terminate(Terminator::Goto(header));
        self.labels.pop();
        self.current = exit;
    }

    /// Expands `for pat in EXPR { .. }`: the preheader (current block)
    /// gains a synthetic `let mut __iter{k} = IntoIterator::into_iter(EXPR);`
    /// statement and the header dispatches on `__iter{k}.next()` via an
    /// `IterNext` terminator. The iterator is stored in the state with
    /// the concrete type `<T as IntoIterator>::IntoIter`, so `T` (the
    /// type of EXPR) must be syntactically known when the loop crosses
    /// a suspension point. Exception: an `a..=b` head is stored as the
    /// generated `__RangeInclusiveIter` wrapper instead of
    /// `RangeInclusive`, whose serde impl drops the internal exhaustion
    /// flag (see `expand::range_inclusive_iter_def`).
    fn lower_for(&mut self, ef: &syn::ExprForLoop) {
        self.check_no_yield(&ef.expr, ERR_FOR_HEAD);
        self.check_for_local_borrow(&ef.expr);
        self.record_expr_uses(&ef.expr, self.current);
        let iter_ident = syn::Ident::new(&format!("__iter{}", self.for_count), ef.expr.span());
        self.for_count += 1;
        let head = &*ef.expr;
        let head_ty = self.infer_ty_source(head);
        let inclusive = self.is_inclusive_range(&head_ty);
        let stmt: syn::Stmt = if inclusive {
            syn::parse_quote! {
                let mut #iter_ident = __RangeInclusiveIter::new(#head);
            }
        } else {
            syn::parse_quote! {
                let mut #iter_ident = ::core::iter::IntoIterator::into_iter(#head);
            }
        };
        self.blocks[self.current].stmts.push(stmt);
        let iter_id = self.define(&iter_ident, self.current, BindingKind::ForIter, None);
        let b = &mut self.bindings[iter_id.0];
        b.mutability = Some(syn::Token![mut](ef.expr.span()));
        b.ty = if inclusive {
            TySource::RangeInclusiveIter(Box::new(head_ty))
        } else {
            TySource::IntoIter(Box::new(head_ty))
        };

        self.with_loop_frame(
            &ef.label,
            &ef.body.stmts,
            TailCtx::Discard,
            |lw, header| {
                let body = lw.new_block(false);
                let exit = lw.new_block(false);
                // The `next()` call consumes the iterator at the header.
                lw.blocks[header].uses.insert(iter_id);
                lw.set_terminator(
                    header,
                    Terminator::IterNext {
                        iter: iter_ident,
                        pat: Box::new((*ef.pat).clone()),
                        body,
                        exit,
                    },
                );
                (body, exit)
            },
            |lw, body| match &*ef.pat {
                // A simple-identifier loop variable gets the iterator's item
                // type; destructured components have no derivable type and
                // must not cross a state boundary.
                syn::Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => {
                    // Bound by the `IterNext` terminator's pattern, not a
                    // `let` statement, so it has no `def_stmt`.
                    let id = lw.define(&pi.ident, body, BindingKind::Local, None);
                    let b = &mut lw.bindings[id.0];
                    b.mutability = pi.mutability;
                    b.ty = TySource::IterItem(iter_id);
                }
                other => lw.define_pat_bindings(other, body, BindingKind::ForPat, None),
            },
        );
    }

    /// Whether the source is an `a..=b` range expression the macro
    /// itself typed as `RangeInclusive`, followed through `let y = x;`
    /// moves. Only these heads are wrapped: a user-annotated
    /// `RangeInclusive` type is taken at face value, like any other
    /// annotation.
    fn is_inclusive_range(&self, src: &TySource) -> bool {
        match src {
            TySource::Range { inclusive, .. } => *inclusive,
            TySource::Moved(id) => self.is_inclusive_range(&self.bindings[id.0].ty),
            _ => false,
        }
    }

    /// Rejects `for x in &local` / `for x in &mut local` where `local`
    /// is a body-local binding: the stored iterator would borrow another
    /// field of the same state. Borrows of arguments point outside the
    /// state and are fine; method calls (`local.iter()`) are left to
    /// borrowck.
    fn check_for_local_borrow(&mut self, expr: &syn::Expr) {
        let e = strip_parens(expr);
        if let syn::Expr::Reference(r) = e
            && let syn::Expr::Path(p) = &*r.expr
            && p.qself.is_none()
            && let Some(ident) = p.path.get_ident()
            && let Some(id) = self.resolve(&ident.to_string())
            && self.bindings[id.0].kind != BindingKind::Arg
        {
            self.err(syn::Error::new_spanned(expr, for_local_borrow_err(ident)));
        }
    }

    /// A block statement or block-valued initializer/tail: the contents
    /// are lowered in place with the block's own trailing expression
    /// handled per `ctx`. A labeled block additionally becomes a `break`
    /// target whose `break` values follow the same destination.
    fn lower_block_stmt(&mut self, eb: &syn::ExprBlock, ctx: TailCtx) {
        match &eb.label {
            Some(label) => {
                let join = self.new_block(false);
                self.labels.push(Frame {
                    label: Some(label.name.ident.to_string()),
                    header: None,
                    dest: BreakDest::of(ctx, join),
                });
                self.in_scope(|lw| lw.lower_stmt_list(&eb.block.stmts, ctx));
                self.goto_if_open(join);
                self.labels.pop();
                self.current = join;
            }
            None => {
                self.in_scope(|lw| lw.lower_stmt_list(&eb.block.stmts, ctx));
            }
        }
    }
}

/// Turns a match arm body into a statement for lowering: the arm's value
/// is discarded in statement-position matches, so this is just the
/// discarded-expression wrapping rule.
fn wrap_arm_body(body: &syn::Expr) -> syn::Stmt {
    stmt_from_discarded_expr(body.clone())
}
