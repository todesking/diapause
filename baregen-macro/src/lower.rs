//! Lowers a coroutine body into a control-flow graph.
//!
//! Only statements that transitively contain `yield_!` are expanded into
//! CFG structure; every other statement — control flow included — is kept
//! as an opaque statement inside a basic block.

use std::collections::{BTreeSet, HashMap, HashSet};

use syn::spanned::Spanned;
use syn::visit::Visit;

use crate::cfg::{
    Binding, BindingId, BindingKind, Block, BlockId, BorrowSource, Cfg, MatchArm, ResumeBinding,
    Terminator, TySource, simplify,
};
use crate::ty_infer::strip_parens;

/// The syntactic form of a `yield_!` resume binding, before it has been
/// assigned a `BindingId`.
struct ResumeBindingSpec {
    ident: syn::Ident,
    mutability: Option<syn::Token![mut]>,
    ty: Option<syn::Type>,
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
/// the entry block without being defined by it.
pub fn lower(args: &[syn::Ident], body: &syn::Block) -> syn::Result<Cfg> {
    let mut lw = Lowerer::new(args);
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
    scopes: Vec<HashMap<String, BindingId>>,
    labels: Vec<Frame>,
    errors: ErrorSink,
    current: BlockId,
    /// Number of `__iter{k}` bindings created so far.
    for_count: usize,
}

impl Lowerer {
    fn new(args: &[syn::Ident]) -> Self {
        let mut lw = Lowerer {
            blocks: Vec::new(),
            bindings: Vec::new(),
            scopes: vec![HashMap::new()],
            labels: Vec::new(),
            errors: ErrorSink::default(),
            current: 0,
            for_count: 0,
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
                inline: false,
            })
            .collect();
        let mut cfg = Cfg {
            blocks,
            entry: 0,
            bindings: self.bindings,
        };
        simplify(&mut cfg);
        Ok(cfg)
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

const ERR_STMT_POSITION: &str = "yield_! is only allowed in statement position: \
     `yield_!(expr);` or `let x = yield_!(expr);`";
const ERR_TRAILING_YIELD: &str =
    "yield_! as the trailing expression is not supported; add a semicolon";
const ERR_FOREIGN_MACRO: &str = "yield_! cannot appear inside another macro invocation";
const ERR_LET_ELSE_INIT: &str =
    "yield_! in the initializer of `let ... else` is not supported";
const ERR_SIMPLE_BINDING: &str =
    "the binding of `let ... = yield_!(...)` must be a simple identifier";
const ERR_VALUE_LET_BINDING: &str =
    "the binding of a `let` whose initializer contains yield_! must be a simple identifier";
const ERR_YIELD_ARG: &str = "yield_! takes a single expression";
const ERR_VALUE_POSITION: &str = "yield_! in value position is only supported when the \
     value is the whole `let` initializer or function tail expression and is an \
     `if`/`match`/`loop`/block expression whose arms produce the value; as a workaround, \
     declare `let mut x: Option<T> = None;` before the control flow, assign \
     `x = Some(...);` where the value is produced, and use `x.unwrap()` afterwards";
const ERR_TAIL: &str = "yield_! in the trailing expression is not supported here; \
     add a semicolon";
const ERR_COND: &str = "yield_! in a condition expression is not supported";
const ERR_SCRUTINEE: &str = "yield_! in a match scrutinee is not supported";
const ERR_GUARD: &str = "yield_! in a match guard is not supported";
const ERR_UNSAFE: &str = "yield_! inside an unsafe block is not supported";
const ERR_LET_CHAIN: &str = "a let-chain condition is not supported when the body contains \
     yield_!; use nested `if let` or `match` instead";
const ERR_FOR_HEAD: &str = "yield_! in a `for` loop's iterator expression is not supported";
const ERR_BREAK_VALUE: &str = "`break` with a value can only target a loop containing \
     yield_! when the loop is a `let` initializer or the function's trailing expression";

fn for_local_borrow_err(name: &syn::Ident) -> String {
    format!(
        "cannot iterate over a borrow of the local variable `{name}`: the iterator would be \
         stored in the coroutine state alongside `{name}` itself, making the state \
         self-referential; iterate by value instead"
    )
}

fn opaque_jump_err(kw: &str) -> String {
    format!(
        "`{kw}` cannot target a loop containing yield_! from inside a statement \
         that does not contain yield_!"
    )
}

// === Yield detection ===

pub fn is_yield_macro(mac: &syn::Macro) -> bool {
    mac.path
        .segments
        .last()
        .is_some_and(|seg| seg.ident == "yield_")
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
            proc_macro2::TokenTree::Group(g) if tokens_contain_yield(g.stream()) => {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// Skips closures, async blocks, and nested items when visiting a
/// coroutine body: they are separate scopes, so `yield_!`, `?`, and
/// break/continue inside them must not be attributed to the enclosing
/// coroutine. Works for both `syn::visit::Visit` and
/// `syn::visit_mut::VisitMut` impls.
macro_rules! skip_nested_scopes {
    (Visit) => {
        fn visit_expr_closure(&mut self, _: &'ast syn::ExprClosure) {}
        fn visit_expr_async(&mut self, _: &'ast syn::ExprAsync) {}
        fn visit_item(&mut self, _: &'ast syn::Item) {}
    };
    (VisitMut) => {
        fn visit_expr_closure_mut(&mut self, _: &mut syn::ExprClosure) {}
        fn visit_expr_async_mut(&mut self, _: &mut syn::ExprAsync) {}
        fn visit_item_mut(&mut self, _: &mut syn::Item) {}
    };
}
pub(crate) use skip_nested_scopes;

/// Finds genuine `yield_!` invocations. Closures, async blocks, and
/// nested items are separate scopes and pass through. Foreign macros
/// whose tokens mention yield_! do not count as containing a yield;
/// they are rejected separately.
#[derive(Default)]
struct ContainsYield {
    found: bool,
}

impl<'ast> Visit<'ast> for ContainsYield {
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if is_yield_macro(mac) {
            self.found = true;
        }
    }

    skip_nested_scopes!(Visit);
}

fn contains_yield_stmt(stmt: &syn::Stmt) -> bool {
    let mut c = ContainsYield::default();
    c.visit_stmt(stmt);
    c.found
}

fn contains_yield_expr(expr: &syn::Expr) -> bool {
    let mut c = ContainsYield::default();
    c.visit_expr(expr);
    c.found
}

/// Reports every `yield_!` with a position-specific message and every
/// foreign macro carrying yield_! tokens.
struct YieldBan<'a> {
    msg: &'a str,
    error: ErrorSink,
}

impl YieldBan<'_> {
    fn record(&mut self, e: syn::Error) {
        self.error.push(e);
    }
}

impl<'ast> Visit<'ast> for YieldBan<'_> {
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if is_yield_macro(mac) {
            self.record(syn::Error::new_spanned(mac, self.msg));
        } else if tokens_contain_yield(mac.tokens.clone()) {
            self.record(syn::Error::new(mac.span(), ERR_FOREIGN_MACRO));
        }
    }

    skip_nested_scopes!(Visit);
}

// === Small collectors ===

/// Collects identifiers that may refer to local variables. Overapproximates:
/// every unqualified single-segment
/// path counts, and all identifiers inside macro invocations are taken
/// verbatim from the token stream.
#[derive(Default)]
pub(crate) struct UseCollector {
    pub(crate) used: HashSet<String>,
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

pub(crate) fn collect_token_idents(tokens: proc_macro2::TokenStream, out: &mut HashSet<String>) {
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

/// Collects the identifiers a pattern binds, in visit order.
#[derive(Default)]
pub(crate) struct PatBindingCollector {
    pub(crate) bindings: Vec<(syn::Ident, Option<syn::Token![mut]>)>,
}

impl<'ast> Visit<'ast> for PatBindingCollector {
    fn visit_pat_ident(&mut self, pi: &'ast syn::PatIdent) {
        self.bindings.push((pi.ident.clone(), pi.mutability));
        syn::visit::visit_pat_ident(self, pi);
    }
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
                let span = stmts.last().map_or_else(proc_macro2::Span::call_site, |s| s.span());
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
    /// arms; anything else cannot suspend and is an error.
    fn lower_value_expr(&mut self, e: &syn::Expr, ctx: TailCtx) {
        match e {
            syn::Expr::If(ei) => self.lower_if(ei, ctx),
            syn::Expr::Match(em) => self.lower_match(em, ctx),
            syn::Expr::Loop(el) => self.lower_loop(el, ctx),
            syn::Expr::Block(eb) => self.lower_block_stmt(eb, ctx),
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
                self.lower_yield(&sm.mac, None);
            }
            // The same, reconstructed as an expression statement (arm
            // bodies are wrapped this way).
            syn::Stmt::Expr(syn::Expr::Macro(em), semi) if is_yield_macro(&em.mac) => {
                if semi.is_none() {
                    return self.err(syn::Error::new_spanned(em, ERR_TRAILING_YIELD));
                }
                self.lower_yield(&em.mac, None);
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
            syn::Stmt::Expr(syn::Expr::Break(b), _) => self.lower_break(b),
            syn::Stmt::Expr(syn::Expr::Continue(c), _) => self.lower_continue(c),
            _ if !contains_yield_stmt(stmt) => self.push_opaque(stmt),
            // From here on the statement contains a yield_! somewhere.
            syn::Stmt::Local(local)
                if matches!(&local.init, Some(init) if init.diverge.is_some()) =>
            {
                self.lower_let_else(local);
            }
            syn::Stmt::Local(local) => self.lower_let_value(local),
            syn::Stmt::Expr(e, _) => self.lower_control_expr(e),
            // Stmt::Macro with yield tokens is caught as opaque; items
            // never contain our yield.
            _ => self.push_opaque(stmt),
        }
    }

    fn lower_let_yield(&mut self, local: &syn::Local) {
        let init = local.init.as_ref().expect("BUG: checked by caller");
        if init.diverge.is_some() {
            return self.err(syn::Error::new_spanned(&init.expr, ERR_LET_ELSE_INIT));
        }
        let syn::Expr::Macro(m) = &*init.expr else {
            unreachable!()
        };
        let (pat, ty) = match &local.pat {
            syn::Pat::Type(pt) => (&*pt.pat, Some((*pt.ty).clone())),
            other => (other, None),
        };
        let binding = match pat {
            syn::Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => ResumeBindingSpec {
                ident: pi.ident.clone(),
                mutability: pi.mutability,
                ty,
            },
            other => {
                return self.err(syn::Error::new_spanned(other, ERR_SIMPLE_BINDING));
            }
        };
        self.lower_yield(&m.mac, Some(binding));
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
        let init = local.init.as_ref().expect("BUG: a yield is inside the initializer");
        let expr = strip_parens(&init.expr);
        if !matches!(
            expr,
            syn::Expr::If(_)
                | syn::Expr::Match(_)
                | syn::Expr::Loop(_)
                | syn::Expr::While(_)
                | syn::Expr::ForLoop(_)
                | syn::Expr::Block(_)
                | syn::Expr::Unsafe(_)
        ) {
            return self.err(syn::Error::new_spanned(local, ERR_VALUE_POSITION));
        }
        let (pat, annotation) = match &local.pat {
            syn::Pat::Type(pt) => (&*pt.pat, Some((*pt.ty).clone())),
            other => (other, None),
        };
        // `let _ = <init>;` binds nothing: the arm values are simply
        // discarded, like a statement-position construct.
        if matches!(pat, syn::Pat::Wild(_)) {
            return self.lower_value_expr(expr, TailCtx::Discard);
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
        self.lower_value_expr(expr, TailCtx::Store(id));
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
            self.terminate(Terminator::Return(syn::parse_quote_spanned!(
                else_token.span() =>
                    ::core::unreachable!("the `else` block of `let ... else` must diverge")
            )));
        }
        self.current = cont;
        self.define_pat_bindings(pat, cont, BindingKind::ArmPat, None);
    }

    /// Ends the current block with a `Yield` terminator and switches to
    /// the resume-point continuation block.
    fn lower_yield(&mut self, mac: &syn::Macro, binding: Option<ResumeBindingSpec>) {
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
        self.check_no_yield(&value, ERR_STMT_POSITION);
        self.record_expr_uses(&value, self.current);
        let next = self.new_block(true);
        let resume_binding = binding.map(|spec| {
            let id = self.define(&spec.ident, next, BindingKind::Resume, None);
            let b = &mut self.bindings[id.0];
            b.mutability = spec.mutability;
            if let Some(t) = &spec.ty {
                b.ty = TySource::Known(t.clone());
            }
            ResumeBinding {
                binding: id,
                mutability: spec.mutability,
                ty: spec.ty,
            }
        });
        self.terminate(Terminator::Yield {
            value,
            resume_binding,
            next,
        });
        self.current = next;
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

    /// Appends a statement without yield_! to the current block.
    fn push_opaque(&mut self, stmt: &syn::Stmt) {
        self.validate_opaque(stmt);
        self.record_stmt_uses(stmt);
        if let syn::Stmt::Local(local) = stmt {
            // The statement is not pushed yet, so its future index in
            // `stmts` is the current length.
            let stmt_idx = self.blocks[self.current].stmts.len();
            self.define_local(local, stmt_idx);
        }
        self.blocks[self.current].stmts.push(stmt.clone());
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
fn is_let_chain(cond: &syn::Expr) -> bool {
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
        match e {
            syn::Expr::If(ei) => self.lower_if(ei, TailCtx::Discard),
            syn::Expr::Match(em) => self.lower_match(em, TailCtx::Discard),
            syn::Expr::Loop(el) => self.lower_loop(el, TailCtx::Discard),
            syn::Expr::While(ew) => self.lower_while(ew),
            syn::Expr::ForLoop(ef) => self.lower_for(ef),
            syn::Expr::Block(eb) => self.lower_block_stmt(eb, TailCtx::Discard),
            syn::Expr::Unsafe(eu) => {
                self.err(syn::Error::new_spanned(eu.unsafe_token, ERR_UNSAFE));
            }
            other => self.err(syn::Error::new_spanned(other, ERR_STMT_POSITION)),
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
        if is_let_chain(&ei.cond) {
            self.err(syn::Error::new_spanned(ei.if_token, ERR_LET_CHAIN));
            self.terminate(Terminator::Goto(join));
            return;
        }
        let then_bb = self.new_block(false);
        // Without an `else`, the false edge produces `()`: in `Store`
        // context it needs a block that assigns it (otherwise the
        // binding would look live at the function entry).
        let else_bb = match (&ei.else_branch, ctx) {
            (None, TailCtx::Store(_)) | (Some(_), _) => self.new_block(false),
            (None, _) => join,
        };
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
            return self.err(syn::Error::new_spanned(ew.while_token, ERR_LET_CHAIN));
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

    /// Expands `for pat in EXPR { .. }`: the preheader (current block)
    /// gains a synthetic `let mut __iter{k} = IntoIterator::into_iter(EXPR);`
    /// statement and the header dispatches on `__iter{k}.next()` via an
    /// `IterNext` terminator. The iterator is stored in the state with
    /// the concrete type `<T as IntoIterator>::IntoIter`, so `T` (the
    /// type of EXPR) must be syntactically known when the loop crosses
    /// a suspension point.
    fn lower_for(&mut self, ef: &syn::ExprForLoop) {
        self.check_no_yield(&ef.expr, ERR_FOR_HEAD);
        self.check_for_local_borrow(&ef.expr);
        self.record_expr_uses(&ef.expr, self.current);
        let iter_ident = syn::Ident::new(&format!("__iter{}", self.for_count), ef.expr.span());
        self.for_count += 1;
        let head = &*ef.expr;
        let stmt: syn::Stmt = syn::parse_quote! {
            let mut #iter_ident = ::core::iter::IntoIterator::into_iter(#head);
        };
        self.blocks[self.current].stmts.push(stmt);
        let head_ty = self.infer_ty_source(head);
        let iter_id = self.define(&iter_ident, self.current, BindingKind::ForIter, None);
        let b = &mut self.bindings[iter_id.0];
        b.mutability = Some(syn::Token![mut](ef.expr.span()));
        b.ty = TySource::IntoIter(Box::new(head_ty));

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

// === Opaque statement validation ===

impl Lowerer {
    /// Checks a statement that stays opaque: no foreign macro may carry
    /// yield_! tokens, and no `break`/`continue` inside it may target an
    /// expanded (yield-containing) loop or labeled block outside it.
    fn validate_opaque(&mut self, stmt: &syn::Stmt) {
        let expanded_labels: HashSet<String> =
            self.labels.iter().filter_map(|f| f.label.clone()).collect();
        let mut checker = OpaqueChecker {
            expanded_labels,
            in_expanded_loop: self.labels.iter().any(|f| f.header.is_some()),
            local_loop_depth: 0,
            local_labels: Vec::new(),
            error: ErrorSink::default(),
        };
        checker.visit_stmt(stmt);
        if let Some(e) = checker.error.into_option() {
            self.err(e);
        }
    }
}

struct OpaqueChecker {
    /// Labels of expanded loops and blocks enclosing the statement.
    expanded_labels: HashSet<String>,
    /// Whether any expanded loop encloses the statement.
    in_expanded_loop: bool,
    /// Loops of the statement itself that enclose the current node.
    local_loop_depth: usize,
    /// Labels declared within the statement (shadow expanded ones).
    local_labels: Vec<String>,
    error: ErrorSink,
}

impl OpaqueChecker {
    fn record(&mut self, e: syn::Error) {
        self.error.push(e);
    }

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

    fn check_jump(&mut self, label: &Option<syn::Lifetime>, kw: &str, span_of: &dyn quote::ToTokens) {
        match label {
            Some(l) => {
                let name = l.ident.to_string();
                if !self.local_labels.contains(&name) && self.expanded_labels.contains(&name) {
                    self.record(syn::Error::new_spanned(span_of, opaque_jump_err(kw)));
                }
            }
            None => {
                if self.local_loop_depth == 0 && self.in_expanded_loop {
                    self.record(syn::Error::new_spanned(span_of, opaque_jump_err(kw)));
                }
            }
        }
    }
}

impl<'ast> Visit<'ast> for OpaqueChecker {
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if is_yield_macro(mac) {
            // Unreachable in practice: the caller only validates
            // statements without yield_!. Kept for robustness.
            self.record(syn::Error::new_spanned(mac, ERR_STMT_POSITION));
        } else if tokens_contain_yield(mac.tokens.clone()) {
            self.record(syn::Error::new(mac.span(), ERR_FOREIGN_MACRO));
        }
    }

    fn visit_expr_loop(&mut self, node: &'ast syn::ExprLoop) {
        self.enter_loop(&node.label, |v| syn::visit::visit_expr_loop(v, node));
    }

    fn visit_expr_while(&mut self, node: &'ast syn::ExprWhile) {
        self.enter_loop(&node.label, |v| syn::visit::visit_expr_while(v, node));
    }

    fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
        self.enter_loop(&node.label, |v| syn::visit::visit_expr_for_loop(v, node));
    }

    fn visit_expr_block(&mut self, node: &'ast syn::ExprBlock) {
        if let Some(l) = &node.label {
            self.local_labels.push(l.name.ident.to_string());
            syn::visit::visit_expr_block(self, node);
            self.local_labels.pop();
        } else {
            syn::visit::visit_expr_block(self, node);
        }
    }

    fn visit_expr_break(&mut self, node: &'ast syn::ExprBreak) {
        self.check_jump(&node.label, "break", node);
        syn::visit::visit_expr_break(self, node);
    }

    fn visit_expr_continue(&mut self, node: &'ast syn::ExprContinue) {
        self.check_jump(&node.label, "continue", node);
        syn::visit::visit_expr_continue(self, node);
    }

    // Separate scopes: break/continue and yield_! inside these belong to
    // them, not to the coroutine.
    skip_nested_scopes!(Visit);
}

/// Turns a match arm body into a statement for lowering: the arm's value
/// is discarded in statement-position matches, so this is just the
/// discarded-expression wrapping rule.
fn wrap_arm_body(body: &syn::Expr) -> syn::Stmt {
    stmt_from_discarded_expr(body.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::lower_args;
    use syn::parse_quote;

    fn lower_ok(block: &syn::Block) -> Cfg {
        lower_args(&[], block)
    }

    fn error_of(block: &syn::Block) -> syn::Error {
        lower(&[], block).unwrap_err()
    }

    /// The BindingId of the (unique) binding with this name.
    fn binding(cfg: &Cfg, name: &str) -> BindingId {
        let matches: Vec<BindingId> = cfg
            .bindings
            .iter()
            .enumerate()
            .filter(|(_, b)| b.ident == name)
            .map(|(i, _)| BindingId(i))
            .collect();
        assert_eq!(matches.len(), 1, "binding `{name}` not unique: {matches:?}");
        matches[0]
    }

    fn ids(items: &[BindingId]) -> BTreeSet<BindingId> {
        items.iter().copied().collect()
    }

    // === Straight-line code ===

    #[test]
    fn no_yield_is_a_single_block() {
        let block: syn::Block = parse_quote!({
            let x = 1;
            x + 1
        });
        let cfg = lower_ok(&block);
        assert_eq!(cfg.blocks.len(), 1);
        assert_eq!(cfg.entry, 0);
        let b = &cfg.blocks[0];
        assert_eq!(b.stmts.len(), 1);
        let expected: syn::Expr = parse_quote!(x + 1);
        assert!(matches!(&b.terminator, Terminator::Return(e) if *e == expected));
    }

    #[test]
    fn straight_line_yields_split_into_expected_blocks() {
        let block: syn::Block = parse_quote!({
            let a = 1;
            yield_!(a);
            let r = yield_!(2);
            r
        });
        let cfg = lower_ok(&block);
        assert_eq!(cfg.blocks.len(), 3);
        assert_eq!(cfg.entry, 0);
        assert_eq!(cfg.blocks[0].stmts.len(), 1);
        assert!(!cfg.blocks[0].resume_point);
        let Terminator::Yield {
            resume_binding: None,
            next: 1,
            ..
        } = &cfg.blocks[0].terminator
        else {
            panic!("expected plain yield: {:?}", cfg.blocks[0].terminator);
        };
        assert!(cfg.blocks[1].resume_point);
        assert!(cfg.blocks[1].stmts.is_empty());
        let Terminator::Yield {
            resume_binding: Some(rb),
            next: 2,
            ..
        } = &cfg.blocks[1].terminator
        else {
            panic!("expected binding yield: {:?}", cfg.blocks[1].terminator);
        };
        assert_eq!(rb.binding, binding(&cfg, "r"));
        assert!(rb.mutability.is_none());
        assert!(rb.ty.is_none());
        assert!(cfg.blocks[2].resume_point);
        assert!(matches!(cfg.blocks[2].terminator, Terminator::Return(_)));
        // r is defined by the resume transition into block 2, so it is
        // not an upward-exposed use there.
        assert_eq!(cfg.blocks[2].defs, ids(&[binding(&cfg, "r")]));
        assert!(cfg.blocks[2].uses.is_empty());
    }

    #[test]
    fn typed_and_mut_resume_binding() {
        let block: syn::Block = parse_quote!({
            let mut r: String = yield_!(1);
            drop(r);
        });
        let cfg = lower_ok(&block);
        let Terminator::Yield {
            resume_binding: Some(rb),
            ..
        } = &cfg.blocks[0].terminator
        else {
            panic!("expected binding yield");
        };
        assert_eq!(cfg.bindings[rb.binding.0].ident, "r");
        assert!(rb.mutability.is_some());
        let expected: syn::Type = parse_quote!(String);
        assert_eq!(rb.ty, Some(expected));
    }

    #[test]
    fn empty_yield_value_is_unit() {
        let block: syn::Block = parse_quote!({
            yield_!();
        });
        let cfg = lower_ok(&block);
        let unit: syn::Expr = parse_quote!(());
        assert!(matches!(&cfg.blocks[0].terminator, Terminator::Yield { value, .. } if *value == unit));
    }

    #[test]
    fn body_without_tail_returns_unit() {
        let block: syn::Block = parse_quote!({
            yield_!(1);
        });
        let cfg = lower_ok(&block);
        assert_eq!(cfg.blocks.len(), 2);
        let unit: syn::Expr = parse_quote!(());
        assert!(matches!(&cfg.blocks[1].terminator, Terminator::Return(e) if *e == unit));
    }

    // === Control-flow expansion ===

    #[test]
    fn if_with_yield_branches_and_joins() {
        let block: syn::Block = parse_quote!({
            let mut acc: u32 = 0;
            if c {
                yield_!(1);
                acc += 1;
            }
            f(acc);
        });
        let cfg = lower_args(&["c"], &block);
        // 0: entry [let acc; Branch] -> then=2, else=join=1
        // 2: then (inline) [Yield] -> 3
        // 3: resume [acc += 1; Goto 1]
        // 1: join [f(acc); Return ()]
        assert_eq!(cfg.blocks.len(), 4);
        let acc = binding(&cfg, "acc");
        let c = binding(&cfg, "c");
        let b0 = &cfg.blocks[0];
        assert_eq!(b0.stmts.len(), 1);
        assert_eq!(b0.uses, ids(&[c]));
        assert_eq!(b0.defs, ids(&[acc]));
        assert!(matches!(
            b0.terminator,
            Terminator::Branch { then_: 2, else_: 1, .. }
        ));
        let then_ = &cfg.blocks[2];
        assert!(then_.inline && !then_.resume_point);
        assert!(then_.stmts.is_empty());
        assert!(matches!(then_.terminator, Terminator::Yield { next: 3, .. }));
        let resume = &cfg.blocks[3];
        assert!(resume.resume_point && !resume.inline);
        assert_eq!(resume.uses, ids(&[acc]));
        assert!(matches!(resume.terminator, Terminator::Goto(1)));
        let join = &cfg.blocks[1];
        assert!(!join.inline, "a join point must stay a variant");
        assert_eq!(join.uses, ids(&[acc]));
        assert!(matches!(join.terminator, Terminator::Return(_)));
    }

    #[test]
    fn else_if_chain_shares_one_join() {
        let block: syn::Block = parse_quote!({
            if a {
                yield_!(1);
            } else if b {
                yield_!(2);
            } else {
                yield_!(3);
            }
            done();
        });
        let cfg = lower_args(&["a", "b"], &block);
        // Exactly one join: each resume block ends with Goto(join) and
        // the join is the single Return block.
        let mut joins = BTreeSet::new();
        for blk in &cfg.blocks {
            if blk.resume_point {
                let Terminator::Goto(j) = blk.terminator else {
                    panic!("resume should go to the join: {:?}", blk.terminator);
                };
                joins.insert(j);
            }
        }
        assert_eq!(joins.len(), 1);
        let join = *joins.first().unwrap();
        assert!(matches!(cfg.blocks[join].terminator, Terminator::Return(_)));
        // entry Branch -> then + nested else-if block Branch -> 2 arms
        let Terminator::Branch { else_, .. } = &cfg.blocks[cfg.entry].terminator else {
            panic!("entry must branch");
        };
        assert!(matches!(
            cfg.blocks[*else_].terminator,
            Terminator::Branch { .. }
        ));
    }

    #[test]
    fn while_loop_matches_design_shape() {
        let block: syn::Block = parse_quote!({
            let mut sum: u32 = 0;
            let mut i: u32 = 0;
            while i < n {
                let r = yield_!(sum);
                sum += r;
                i += 1;
            }
            sum
        });
        let cfg = lower_args(&["n"], &block);
        // Start [lets; Goto header], header [Branch], body (inline)
        // [Yield], resume [Goto header], exit (inline) [Return sum].
        assert_eq!(cfg.blocks.len(), 5);
        let (n, sum, i, r) = (
            binding(&cfg, "n"),
            binding(&cfg, "sum"),
            binding(&cfg, "i"),
            binding(&cfg, "r"),
        );
        let b0 = &cfg.blocks[0];
        assert_eq!(b0.stmts.len(), 2);
        assert_eq!(b0.defs, ids(&[sum, i]));
        let Terminator::Goto(header) = b0.terminator else {
            panic!("entry should fall into the header");
        };
        let hb = &cfg.blocks[header];
        assert!(!hb.inline, "multi-entry loop header must stay a variant");
        assert_eq!(hb.uses, ids(&[i, n]));
        let Terminator::Branch { then_, else_, .. } = hb.terminator else {
            panic!("while header must branch");
        };
        let body = &cfg.blocks[then_];
        assert!(body.inline);
        assert_eq!(body.uses, ids(&[sum]), "yield value is used pre-transition");
        let Terminator::Yield {
            resume_binding: Some(rb),
            next,
            ..
        } = &body.terminator
        else {
            panic!("loop body should yield");
        };
        assert_eq!(rb.binding, r);
        let resume = &cfg.blocks[*next];
        assert!(resume.resume_point);
        assert_eq!(resume.defs, ids(&[r]));
        assert_eq!(resume.uses, ids(&[sum, i]), "r is defined locally");
        assert!(matches!(resume.terminator, Terminator::Goto(h) if h == header));
        let exit = &cfg.blocks[else_];
        assert!(exit.inline);
        assert_eq!(exit.uses, ids(&[sum]));
        assert!(matches!(&exit.terminator, Terminator::Return(_)));
    }

    #[test]
    fn match_with_yield_expands_arms() {
        let block: syn::Block = parse_quote!({
            let x: u32 = k;
            match x {
                0 => {
                    yield_!(0);
                }
                n2 => {
                    yield_!(1);
                    g(n2);
                }
            }
            done();
        });
        let cfg = lower_args(&["k"], &block);
        assert_eq!(cfg.blocks.len(), 6);
        let Terminator::Match { arms, .. } = &cfg.blocks[0].terminator else {
            panic!("expected match terminator");
        };
        assert_eq!(arms.len(), 2);
        let n2 = binding(&cfg, "n2");
        let arm1 = &cfg.blocks[arms[1].body];
        assert!(arm1.inline);
        assert_eq!(arm1.defs, ids(&[n2]), "pattern binding defined in arm body");
        let Terminator::Yield { next, .. } = arm1.terminator else {
            panic!("second arm should yield");
        };
        // The use of n2 after the yield is recorded in the resume block
        // (task 12 turns this pattern-binding crossing into an error).
        assert_eq!(cfg.blocks[next].uses, ids(&[n2]));
        // Both arms join on the same block.
        let joins: BTreeSet<BlockId> = cfg
            .blocks
            .iter()
            .filter(|b| b.resume_point)
            .map(|b| match b.terminator {
                Terminator::Goto(j) => j,
                _ => panic!("resume should goto the join"),
            })
            .collect();
        assert_eq!(joins.len(), 1);
    }

    #[test]
    fn match_guard_uses_count_in_the_match_block() {
        let block: syn::Block = parse_quote!({
            match v {
                y if y > lim => {
                    yield_!(1);
                }
                _ => {}
            };
        });
        let cfg = lower_args(&["v", "lim"], &block);
        let (v, lim) = (binding(&cfg, "v"), binding(&cfg, "lim"));
        assert_eq!(cfg.blocks[0].uses, ids(&[v, lim]));
        let Terminator::Match { arms, .. } = &cfg.blocks[0].terminator else {
            panic!("expected match");
        };
        assert!(arms[0].guard.is_some());
        assert!(arms[1].guard.is_none());
    }

    #[test]
    fn if_let_becomes_a_match_terminator() {
        let block: syn::Block = parse_quote!({
            if let Some(x2) = opt {
                yield_!(x2);
            }
            done();
        });
        let cfg = lower_args(&["opt"], &block);
        let Terminator::Match { arms, .. } = &cfg.blocks[0].terminator else {
            panic!("if let should lower to a match: {:?}", cfg.blocks[0].terminator);
        };
        assert_eq!(cfg.blocks[0].uses, ids(&[binding(&cfg, "opt")]));
        assert_eq!(arms.len(), 2);
        let wild: syn::Pat = parse_quote!(_);
        assert_eq!(arms[1].pat, wild);
        // The pattern arm binds x2 and yields; without an `else`, the
        // `_` arm goes straight to the join.
        let then_ = &cfg.blocks[arms[0].body];
        assert_eq!(then_.defs, ids(&[binding(&cfg, "x2")]));
        let Terminator::Yield { next, .. } = then_.terminator else {
            panic!("then arm should yield");
        };
        let join = arms[1].body;
        assert!(matches!(cfg.blocks[next].terminator, Terminator::Goto(j) if j == join));
        assert!(matches!(cfg.blocks[join].terminator, Terminator::Return(_)));
    }

    #[test]
    fn else_if_let_chains_into_a_match() {
        let block: syn::Block = parse_quote!({
            if a {
                yield_!(1);
            } else if let Some(x2) = opt {
                yield_!(2);
            } else {
                yield_!(3);
            }
            done();
        });
        let cfg = lower_args(&["a", "opt"], &block);
        let Terminator::Branch { else_, .. } = cfg.blocks[cfg.entry].terminator else {
            panic!("entry must branch");
        };
        let Terminator::Match { arms, .. } = &cfg.blocks[else_].terminator else {
            panic!("else-if-let link should lower to a match");
        };
        assert_eq!(arms.len(), 2);
        // All three arms join on the single Return block.
        let joins: BTreeSet<BlockId> = cfg
            .blocks
            .iter()
            .filter(|b| b.resume_point)
            .map(|b| match b.terminator {
                Terminator::Goto(j) => j,
                _ => panic!("resume should goto the join"),
            })
            .collect();
        assert_eq!(joins.len(), 1);
    }

    #[test]
    fn let_if_let_value_assigns_in_each_arm() {
        let block: syn::Block = parse_quote!({
            let x: u32 = if let Some(v) = opt {
                yield_!(1);
                1
            } else {
                2
            };
            f(x);
        });
        let cfg = lower_args(&["opt"], &block);
        let x = binding(&cfg, "x");
        let Terminator::Match { arms, .. } = &cfg.blocks[0].terminator else {
            panic!("if let should lower to a match");
        };
        // The pattern arm assigns x in its resume block; the `_` arm in
        // its own synthesized block.
        let Terminator::Yield { next, .. } = cfg.blocks[arms[0].body].terminator else {
            panic!("then arm should yield");
        };
        assert!(cfg.blocks[next].defs.contains(&x));
        assert!(cfg.blocks[arms[1].body].defs.contains(&x));
    }

    #[test]
    fn while_let_becomes_a_header_match() {
        let block: syn::Block = parse_quote!({
            while let Some(x2) = it.next() {
                yield_!(x2);
            }
        });
        let cfg = lower_args(&["it"], &block);
        let Terminator::Goto(header) = cfg.blocks[0].terminator else {
            panic!("entry should fall into the header");
        };
        let hb = &cfg.blocks[header];
        assert_eq!(hb.uses, ids(&[binding(&cfg, "it")]));
        let Terminator::Match { arms, .. } = &hb.terminator else {
            panic!("while let header must match");
        };
        assert_eq!(arms.len(), 2);
        let wild: syn::Pat = parse_quote!(_);
        assert_eq!(arms[1].pat, wild);
        let body = &cfg.blocks[arms[0].body];
        assert_eq!(body.defs, ids(&[binding(&cfg, "x2")]));
        assert!(matches!(body.terminator, Terminator::Yield { .. }));
        // exit arm returns
        assert!(matches!(
            cfg.blocks[arms[1].body].terminator,
            Terminator::Return(_)
        ));
    }

    #[test]
    fn let_else_becomes_a_refutable_match() {
        let block: syn::Block = parse_quote!({
            let Some(x2) = opt else {
                yield_!(0);
                return;
            };
            f(x2);
        });
        let cfg = lower_args(&["opt"], &block);
        let Terminator::Match { arms, .. } = &cfg.blocks[0].terminator else {
            panic!("let-else should lower to a match: {:?}", cfg.blocks[0].terminator);
        };
        assert_eq!(cfg.blocks[0].uses, ids(&[binding(&cfg, "opt")]));
        assert_eq!(arms.len(), 2);
        let wild: syn::Pat = parse_quote!(_);
        assert_eq!(arms[1].pat, wild);
        // The pattern arm is the continuation: it binds x2 and returns.
        let cont = &cfg.blocks[arms[0].body];
        assert_eq!(cont.defs, ids(&[binding(&cfg, "x2")]));
        assert!(matches!(cont.terminator, Terminator::Return(_)));
        // The `_` arm yields; its resume block ends in the synthetic
        // unreachable fall-through (the `return;` stays opaque).
        let Terminator::Yield { next, .. } = cfg.blocks[arms[1].body].terminator else {
            panic!("else arm should yield");
        };
        let resume = &cfg.blocks[next];
        assert_eq!(resume.stmts.len(), 1, "the rewritten `return` stays opaque");
        assert!(matches!(resume.terminator, Terminator::Return(_)));
    }

    #[test]
    fn let_else_break_terminates_the_else_arm() {
        let block: syn::Block = parse_quote!({
            loop {
                let Some(x2) = opt else {
                    yield_!(0);
                    break;
                };
                g(x2);
                yield_!(1);
            }
        });
        let cfg = lower_args(&["opt"], &block);
        // The break's resume block jumps to the loop exit; no synthetic
        // unreachable fall-through survives simplification.
        assert!(
            cfg.blocks
                .iter()
                .all(|b| !matches!(&b.terminator, Terminator::Return(e)
                    if quote::quote!(#e).to_string().contains("unreachable"))),
        );
    }

    #[test]
    fn let_else_annotation_moves_to_the_scrutinee() {
        let block: syn::Block = parse_quote!({
            let Some(x2): Option<u32> = opt else {
                yield_!(0);
                return;
            };
            f(x2);
        });
        let cfg = lower_args(&["opt"], &block);
        let Terminator::Match { scrutinee, .. } = &cfg.blocks[0].terminator else {
            panic!("let-else should lower to a match");
        };
        let expected: syn::Expr = parse_quote!({
            let __scrutinee: Option<u32> = opt;
            __scrutinee
        });
        assert_eq!(*scrutinee, expected);
    }

    // === for loops ===

    #[test]
    fn for_loop_matches_design_shape() {
        let block: syn::Block = parse_quote!({
            let mut sum: u32 = 0;
            for i in 0u32..n {
                yield_!(i);
                sum += i;
            }
            sum
        });
        let cfg = lower_args(&["n"], &block);
        // entry [let sum; let __iter0; Goto header], header [IterNext],
        // body (inline) [Yield], resume [sum += i; Goto header],
        // exit (inline) [Return sum].
        assert_eq!(cfg.blocks.len(), 5);
        let (n, sum, it, i) = (
            binding(&cfg, "n"),
            binding(&cfg, "sum"),
            binding(&cfg, "__iter0"),
            binding(&cfg, "i"),
        );
        let b0 = &cfg.blocks[0];
        assert_eq!(b0.stmts.len(), 2, "preheader adds the iterator let");
        assert_eq!(b0.uses, ids(&[n]));
        assert_eq!(b0.defs, ids(&[sum, it]));
        let Terminator::Goto(header) = b0.terminator else {
            panic!("entry should fall into the header");
        };
        let hb = &cfg.blocks[header];
        assert!(!hb.inline, "loop header must stay a variant");
        assert_eq!(hb.uses, ids(&[it]), "next() consumes the iterator");
        let Terminator::IterNext { iter, body, exit, .. } = &hb.terminator else {
            panic!("for header must be IterNext: {:?}", hb.terminator);
        };
        assert_eq!(iter, "__iter0");
        // The iterator binding: synthetic, mutable, IntoIter of the head.
        let ib = &cfg.bindings[it.0];
        assert_eq!(ib.kind, BindingKind::ForIter);
        assert!(ib.mutability.is_some());
        assert!(matches!(
            &ib.ty,
            TySource::IntoIter(inner) if matches!(**inner, TySource::Range { .. })
        ));
        // The loop variable: item type of the iterator.
        assert!(matches!(cfg.bindings[i.0].ty, TySource::IterItem(id) if id == it));
        let bb = &cfg.blocks[*body];
        assert!(bb.inline);
        assert_eq!(bb.defs, ids(&[i]));
        let Terminator::Yield { next, .. } = bb.terminator else {
            panic!("loop body should yield");
        };
        let resume = &cfg.blocks[next];
        assert!(resume.resume_point);
        assert_eq!(resume.uses, ids(&[sum, i]));
        assert!(matches!(resume.terminator, Terminator::Goto(h) if h == header));
        let eb = &cfg.blocks[*exit];
        assert!(eb.inline);
        assert!(matches!(&eb.terminator, Terminator::Return(_)));
    }

    #[test]
    fn for_break_and_continue_target_exit_and_header() {
        let block: syn::Block = parse_quote!({
            for i in 0u32..3 {
                let stop = yield_!(i);
                if stop {
                    yield_!(9);
                    break;
                }
                yield_!(8);
                continue;
            }
            after();
        });
        let cfg = lower_ok(&block);
        let header = cfg
            .blocks
            .iter()
            .position(|b| matches!(b.terminator, Terminator::IterNext { .. }))
            .unwrap();
        let Terminator::IterNext { exit, .. } = cfg.blocks[header].terminator else {
            unreachable!()
        };
        // break resume -> exit, continue resume -> header.
        let gotos: Vec<BlockId> = cfg
            .blocks
            .iter()
            .filter(|b| b.resume_point)
            .filter_map(|b| match b.terminator {
                Terminator::Goto(t) => Some(t),
                _ => None,
            })
            .collect();
        assert!(gotos.contains(&exit));
        assert!(gotos.contains(&header));
    }

    #[test]
    fn labeled_for_break_crosses_inner_for() {
        let block: syn::Block = parse_quote!({
            'outer: for i in 0u32..3 {
                for j in 0u32..3 {
                    yield_!(i + j);
                    break 'outer;
                }
            }
        });
        let cfg = lower_ok(&block);
        // Distinct synthetic iterators per loop.
        binding(&cfg, "__iter0");
        binding(&cfg, "__iter1");
        let outer_exit = cfg
            .blocks
            .iter()
            .find_map(|b| match &b.terminator {
                Terminator::IterNext { iter, exit, .. } if iter == "__iter0" => Some(*exit),
                _ => None,
            })
            .unwrap();
        let resume = cfg.blocks.iter().find(|b| b.resume_point).unwrap();
        assert!(matches!(resume.terminator, Terminator::Goto(t) if t == outer_exit));
    }

    // === break / continue and simplification ===

    #[test]
    fn loop_break_merges_to_two_blocks() {
        let block: syn::Block = parse_quote!({
            loop {
                yield_!(1);
                break;
            }
        });
        let cfg = lower_ok(&block);
        // The header has a single predecessor after the (unreachable)
        // back edge is dropped, so everything merges into entry + resume.
        assert_eq!(cfg.blocks.len(), 2);
        assert!(matches!(
            cfg.blocks[0].terminator,
            Terminator::Yield { next: 1, .. }
        ));
        assert!(cfg.blocks[1].resume_point);
        assert!(matches!(cfg.blocks[1].terminator, Terminator::Return(_)));
    }

    #[test]
    fn loop_continue_goes_to_the_header() {
        let block: syn::Block = parse_quote!({
            let mut i: i32 = 0;
            loop {
                i += 1;
                yield_!(i);
                continue;
            }
        });
        let cfg = lower_ok(&block);
        // entry [let i; Goto header], header [i += 1; Yield], resume
        // [Goto header]; the loop exit is unreachable and removed.
        assert_eq!(cfg.blocks.len(), 3);
        let Terminator::Goto(header) = cfg.blocks[0].terminator else {
            panic!("entry should fall into the header");
        };
        assert!(matches!(
            cfg.blocks[header].terminator,
            Terminator::Yield { .. }
        ));
        let resume = cfg.blocks.iter().find(|b| b.resume_point).unwrap();
        assert!(matches!(resume.terminator, Terminator::Goto(h) if h == header));
        // No Return block remains: the coroutine never completes.
        assert!(
            cfg.blocks
                .iter()
                .all(|b| !matches!(b.terminator, Terminator::Return(_)))
        );
    }

    #[test]
    fn labeled_break_crosses_expanded_loops() {
        let block: syn::Block = parse_quote!({
            'outer: loop {
                loop {
                    yield_!(1);
                    break 'outer;
                }
            }
        });
        let cfg = lower_ok(&block);
        assert_eq!(cfg.blocks.len(), 2);
        assert!(matches!(
            cfg.blocks[0].terminator,
            Terminator::Yield { next: 1, .. }
        ));
        assert!(matches!(cfg.blocks[1].terminator, Terminator::Return(_)));
    }

    #[test]
    fn break_inside_expanded_if_targets_the_loop_exit() {
        let block: syn::Block = parse_quote!({
            loop {
                if c {
                    yield_!(1);
                    break;
                }
            }
            after();
        });
        let cfg = lower_args(&["c"], &block);
        // entry [Goto header], header [Branch then/else], then (inline)
        // [Yield], resume [Goto exit], else-join [Goto header] (inline).
        let Terminator::Goto(header) = cfg.blocks[cfg.entry].terminator else {
            panic!("entry should fall into the header");
        };
        let Terminator::Branch { then_, else_, .. } = cfg.blocks[header].terminator else {
            panic!("expanded if should branch");
        };
        let Terminator::Yield { next, .. } = cfg.blocks[then_].terminator else {
            panic!("then arm should yield");
        };
        // The loop exit ([after(); Return]) has the resume block as its
        // only predecessor, so it is merged into it.
        let resume = &cfg.blocks[next];
        assert!(resume.resume_point);
        assert_eq!(resume.stmts.len(), 1);
        assert!(matches!(resume.terminator, Terminator::Return(_)));
        // if-join loops back to the header
        assert!(matches!(cfg.blocks[else_].terminator, Terminator::Goto(h) if h == header));
    }

    #[test]
    fn nested_opaque_loop_owns_its_breaks() {
        let block: syn::Block = parse_quote!({
            loop {
                yield_!(1);
                loop {
                    break;
                }
            }
        });
        assert!(lower(&[], &block).is_ok());
    }

    #[test]
    fn block_statement_contents_are_flattened() {
        let block: syn::Block = parse_quote!({
            {
                yield_!(1);
            }
            let a: i32 = 2;
            f(a);
        });
        let cfg = lower_ok(&block);
        assert_eq!(cfg.blocks.len(), 2);
        assert!(cfg.blocks[1].resume_point);
        assert_eq!(cfg.blocks[1].stmts.len(), 2);
        assert_eq!(cfg.blocks[1].defs, ids(&[binding(&cfg, "a")]));
    }

    #[test]
    fn labeled_block_break_jumps_past_it() {
        let block: syn::Block = parse_quote!({
            'b: {
                yield_!(1);
                break 'b;
            }
            after();
        });
        let cfg = lower_ok(&block);
        assert_eq!(cfg.blocks.len(), 2);
        let resume = &cfg.blocks[1];
        assert!(resume.resume_point);
        // The unreachable rest of the labeled block is dropped and the
        // join is merged into the resume block.
        assert_eq!(resume.stmts.len(), 1);
        assert!(matches!(resume.terminator, Terminator::Return(_)));
    }

    // === Binding resolution ===

    #[test]
    fn shadowed_bindings_get_distinct_ids() {
        let block: syn::Block = parse_quote!({
            let x: i32 = 1;
            {
                let x: u32 = 2;
                yield_!(1);
                f(x);
            }
            g(x);
        });
        let cfg = lower_ok(&block);
        let xs: Vec<BindingId> = cfg
            .bindings
            .iter()
            .enumerate()
            .filter(|(_, b)| b.ident == "x")
            .map(|(i, _)| BindingId(i))
            .collect();
        assert_eq!(xs.len(), 2, "each `x` must be its own binding");
        assert_eq!(cfg.blocks[0].defs, ids(&xs));
        // f(x) sees the inner x, g(x) the outer one; both uses land in
        // the resume block.
        assert_eq!(cfg.blocks[1].uses, ids(&xs));
    }

    #[test]
    fn macro_tokens_count_as_uses() {
        let block: syn::Block = parse_quote!({
            let a: i32 = 1;
            yield_!(1);
            println!("{}", a);
        });
        let cfg = lower_ok(&block);
        assert_eq!(cfg.blocks[1].uses, ids(&[binding(&cfg, "a")]));
    }

    #[test]
    fn yield_value_uses_belong_to_the_yielding_block() {
        let block: syn::Block = parse_quote!({
            yield_!(x);
        });
        let cfg = lower_args(&["x"], &block);
        assert_eq!(cfg.blocks[0].uses, ids(&[binding(&cfg, "x")]));
        assert!(cfg.blocks[0].defs.is_empty(), "arguments are not defs");
    }

    #[test]
    fn yield_in_closure_passes_through() {
        let block: syn::Block = parse_quote!({
            let f = || yield_!(1);
        });
        let cfg = lower_ok(&block);
        assert_eq!(cfg.blocks.len(), 1);
        assert!(matches!(cfg.blocks[0].terminator, Terminator::Return(_)));
    }

    // === Value position (let initializers and fn tails) ===

    #[test]
    fn let_if_value_assigns_in_each_arm() {
        let block: syn::Block = parse_quote!({
            let x: u32 = if c {
                yield_!(1);
                1
            } else {
                2
            };
            f(x);
        });
        let cfg = lower_args(&["c"], &block);
        let x = binding(&cfg, "x");
        let Terminator::Branch { then_, else_, .. } = cfg.blocks[0].terminator else {
            panic!("entry must branch");
        };
        // The then arm yields; its resume block assigns x and joins.
        let Terminator::Yield { next, .. } = cfg.blocks[then_].terminator else {
            panic!("then arm should yield: {:?}", cfg.blocks[then_].terminator);
        };
        let resume = &cfg.blocks[next];
        assert_eq!(resume.stmts.len(), 1);
        assert_eq!(resume.defs, ids(&[x]));
        let Terminator::Goto(join) = resume.terminator else {
            panic!("resume should goto the join");
        };
        // The else arm assigns x and joins on the same block.
        let else_b = &cfg.blocks[else_];
        assert_eq!(else_b.defs, ids(&[x]));
        assert_eq!(else_b.stmts.len(), 1);
        assert!(matches!(else_b.terminator, Terminator::Goto(j) if j == join));
        // The join uses x without defining it; the binding knows its type.
        assert_eq!(cfg.blocks[join].uses, ids(&[x]));
        assert!(!cfg.blocks[join].defs.contains(&x));
        let expected: syn::Type = parse_quote!(u32);
        assert!(matches!(&cfg.bindings[x.0].ty, TySource::Known(t) if *t == expected));
    }

    #[test]
    fn let_if_without_else_assigns_unit_on_the_false_edge() {
        let block: syn::Block = parse_quote!({
            let x: () = if c {
                yield_!(1);
            };
            f(x);
        });
        let cfg = lower_args(&["c"], &block);
        let x = binding(&cfg, "x");
        let Terminator::Branch { then_, else_, .. } = cfg.blocks[0].terminator else {
            panic!("entry must branch");
        };
        // The synthesized false edge assigns `()`.
        assert_eq!(cfg.blocks[else_].defs, ids(&[x]));
        assert_eq!(cfg.blocks[else_].stmts.len(), 1);
        // The then arm has no tail expression: its resume block assigns
        // `()` too.
        let Terminator::Yield { next, .. } = cfg.blocks[then_].terminator else {
            panic!("then arm should yield");
        };
        assert_eq!(cfg.blocks[next].defs, ids(&[x]));
    }

    #[test]
    fn let_match_value_assigns_in_each_arm() {
        let block: syn::Block = parse_quote!({
            let x: u32 = match k {
                0 => {
                    yield_!(0);
                    1
                }
                _ => 2,
            };
            f(x);
        });
        let cfg = lower_args(&["k"], &block);
        let x = binding(&cfg, "x");
        let Terminator::Match { arms, .. } = &cfg.blocks[0].terminator else {
            panic!("expected match terminator");
        };
        // The non-block arm body `2` becomes a synthetic assignment.
        let wild = &cfg.blocks[arms[1].body];
        assert_eq!(wild.defs, ids(&[x]));
        assert_eq!(wild.stmts.len(), 1);
    }

    #[test]
    fn let_loop_break_value_assigns_before_the_exit() {
        let block: syn::Block = parse_quote!({
            let x: u32 = loop {
                let r = yield_!(1);
                break r;
            };
            f(x);
        });
        let cfg = lower_ok(&block);
        let x = binding(&cfg, "x");
        let r = binding(&cfg, "r");
        // The resume block assigns x from r; the exit block (merged into
        // it) then uses x and returns.
        let resume = cfg.blocks.iter().find(|b| b.resume_point).unwrap();
        assert_eq!(resume.defs, ids(&[x, r]));
        assert_eq!(resume.stmts.len(), 2, "assignment + f(x) after the merge");
        assert!(matches!(resume.terminator, Terminator::Return(_)));
    }

    #[test]
    fn let_labeled_block_break_value_assigns() {
        let block: syn::Block = parse_quote!({
            let x: u32 = 'b: {
                yield_!(1);
                break 'b 5;
            };
            f(x);
        });
        let cfg = lower_ok(&block);
        let x = binding(&cfg, "x");
        let resume = cfg.blocks.iter().find(|b| b.resume_point).unwrap();
        assert!(resume.defs.contains(&x));
    }

    #[test]
    fn let_nested_if_distributes_assignments_to_leaf_arms() {
        let block: syn::Block = parse_quote!({
            let x: u32 = if a {
                if b {
                    yield_!(1);
                    1
                } else {
                    2
                }
            } else {
                3
            };
            f(x);
        });
        let cfg = lower_args(&["a", "b"], &block);
        let x = binding(&cfg, "x");
        let n_defs = cfg.blocks.iter().filter(|blk| blk.defs.contains(&x)).count();
        assert_eq!(n_defs, 3, "one assignment per leaf arm");
    }

    #[test]
    fn let_value_initializer_sees_the_outer_binding() {
        let block: syn::Block = parse_quote!({
            let x: u32 = 1;
            let x: u32 = if c {
                yield_!(1);
                x + 1
            } else {
                2
            };
            f(x);
        });
        let cfg = lower_args(&["c"], &block);
        let xs: Vec<BindingId> = cfg
            .bindings
            .iter()
            .enumerate()
            .filter(|(_, b)| b.ident == "x")
            .map(|(i, _)| BindingId(i))
            .collect();
        assert_eq!(xs.len(), 2);
        let (outer, inner) = (xs[0], xs[1]);
        // `x + 1` in the resume block reads the outer x; the assignment
        // defines the inner one.
        let resume = cfg.blocks.iter().find(|b| b.resume_point).unwrap();
        assert!(resume.uses.contains(&outer));
        assert!(resume.defs.contains(&inner));
    }

    #[test]
    fn fn_tail_if_arms_return() {
        let block: syn::Block = parse_quote!({
            if c {
                yield_!(1);
                1u32
            } else {
                2u32
            }
        });
        let cfg = lower_args(&["c"], &block);
        let one: syn::Expr = parse_quote!(1u32);
        let two: syn::Expr = parse_quote!(2u32);
        let returns: Vec<&Terminator> = cfg
            .blocks
            .iter()
            .filter(|b| matches!(b.terminator, Terminator::Return(_)))
            .map(|b| &b.terminator)
            .collect();
        assert_eq!(returns.len(), 2, "each arm returns; there is no join");
        assert!(returns.iter().any(|t| matches!(t, Terminator::Return(e) if *e == one)));
        assert!(returns.iter().any(|t| matches!(t, Terminator::Return(e) if *e == two)));
    }

    #[test]
    fn fn_tail_loop_break_value_returns() {
        let block: syn::Block = parse_quote!({
            loop {
                let r = yield_!(1);
                break r;
            }
        });
        let cfg = lower_ok(&block);
        let resume = cfg.blocks.iter().find(|b| b.resume_point).unwrap();
        let expected: syn::Expr = parse_quote!(r);
        assert!(
            matches!(&resume.terminator, Terminator::Return(e) if *e == expected),
            "break value should become the return value: {:?}",
            resume.terminator
        );
    }

    #[test]
    fn let_wildcard_value_is_discarded() {
        let block: syn::Block = parse_quote!({
            let _ = if c {
                yield_!(1);
                1
            } else {
                2
            };
            f();
        });
        let cfg = lower_args(&["c"], &block);
        // No binding is created and no synthetic assignment happens:
        // the only binding is the argument.
        assert_eq!(cfg.bindings.len(), 1);
    }

    #[test]
    fn let_value_destructuring_binding_is_rejected() {
        let block: syn::Block = parse_quote!({
            let (a, b) = if c {
                yield_!(1);
                (1, 2)
            } else {
                (3, 4)
            };
        });
        assert!(error_of(&block).to_string().contains("simple identifier"));
    }

    // === Errors ===

    #[test]
    fn yield_in_expression_is_rejected() {
        let block: syn::Block = parse_quote!({
            f(1, yield_!(2));
        });
        let err = error_of(&block);
        assert!(err.to_string().contains("statement position"));
    }

    #[test]
    fn yield_in_value_position_is_rejected() {
        // The initializer is not itself a control-flow expression, so
        // there is no arm tail to assign from.
        let block: syn::Block = parse_quote!({
            let x: u32 = 1 + if c {
                yield_!(1);
                1
            } else {
                2
            };
        });
        let err = error_of(&block);
        assert!(err.to_string().contains("value position"));
    }

    #[test]
    fn yield_in_conditions_is_rejected() {
        let block: syn::Block = parse_quote!({
            if yield_!(1) {
                f();
            }
        });
        assert!(error_of(&block).to_string().contains("condition"));
        let block: syn::Block = parse_quote!({
            while yield_!(1) {
                f();
            }
        });
        assert!(error_of(&block).to_string().contains("condition"));
    }

    #[test]
    fn yield_in_scrutinee_is_rejected() {
        let block: syn::Block = parse_quote!({
            match yield_!(1) {
                _ => {}
            };
        });
        assert!(error_of(&block).to_string().contains("scrutinee"));
        let block: syn::Block = parse_quote!({
            while let Some(x) = yield_!(1) {
                f(x);
            }
        });
        assert!(error_of(&block).to_string().contains("scrutinee"));
    }

    #[test]
    fn yield_in_match_guard_is_rejected() {
        let block: syn::Block = parse_quote!({
            match v {
                y if yield_!(y) => {
                    yield_!(1);
                }
                _ => {}
            };
        });
        assert!(error_of(&block).to_string().contains("guard"));
    }

    #[test]
    fn bare_trailing_yield_is_rejected() {
        let block: syn::Block = parse_quote!({
            yield_!(1)
        });
        assert!(error_of(&block).to_string().contains("add a semicolon"));
    }

    #[test]
    fn yield_in_unsafe_block_is_rejected() {
        let block: syn::Block = parse_quote!({
            unsafe {
                yield_!(1);
            }
        });
        assert!(error_of(&block).to_string().contains("unsafe"));
    }

    #[test]
    fn let_chain_conditions_are_rejected() {
        let block: syn::Block = parse_quote!({
            if let Some(x) = opt && c {
                yield_!(x);
            }
        });
        assert!(error_of(&block).to_string().contains("let-chain"));
        let block: syn::Block = parse_quote!({
            while let Some(x) = opt && c {
                yield_!(x);
            }
        });
        assert!(error_of(&block).to_string().contains("let-chain"));
    }

    #[test]
    fn yield_in_for_head_expression_is_rejected() {
        let block: syn::Block = parse_quote!({
            for x in f(yield_!(1)) {
                g(x);
            }
        });
        assert!(
            error_of(&block)
                .to_string()
                .contains("iterator expression")
        );
    }

    #[test]
    fn for_over_a_borrowed_local_is_rejected() {
        let block: syn::Block = parse_quote!({
            let v: [u32; 3] = [1, 2, 3];
            for x in &v {
                yield_!(*x);
            }
        });
        assert!(error_of(&block).to_string().contains("self-referential"));
        let block: syn::Block = parse_quote!({
            let mut v: [u32; 3] = [1, 2, 3];
            for x in &mut v {
                yield_!(*x);
            }
        });
        assert!(error_of(&block).to_string().contains("self-referential"));
    }

    #[test]
    fn for_over_a_borrowed_argument_is_allowed() {
        let block: syn::Block = parse_quote!({
            for x in &xs {
                yield_!(1);
            }
        });
        let idents = [syn::Ident::new("xs", proc_macro2::Span::call_site())];
        assert!(lower(&idents, &block).is_ok());
    }

    #[test]
    fn break_with_value_is_rejected_in_statement_loops() {
        let block: syn::Block = parse_quote!({
            loop {
                yield_!(1);
                break 5;
            }
            after();
        });
        assert!(
            error_of(&block)
                .to_string()
                .contains("`break` with a value")
        );
    }

    #[test]
    fn opaque_break_into_expanded_loop_is_rejected() {
        let block: syn::Block = parse_quote!({
            loop {
                yield_!(1);
                if c {
                    break;
                }
            }
        });
        let err = error_of(&block);
        assert!(err.to_string().contains("does not contain yield_!"));
    }

    #[test]
    fn opaque_labeled_jumps_into_expanded_loop_are_rejected() {
        let block: syn::Block = parse_quote!({
            'a: loop {
                yield_!(1);
                loop {
                    break 'a;
                }
            }
        });
        assert!(
            error_of(&block)
                .to_string()
                .contains("does not contain yield_!")
        );
        let block: syn::Block = parse_quote!({
            'a: loop {
                yield_!(1);
                loop {
                    continue 'a;
                }
            }
        });
        assert!(
            error_of(&block)
                .to_string()
                .contains("does not contain yield_!")
        );
    }

    #[test]
    fn local_label_shadows_expanded_label() {
        let block: syn::Block = parse_quote!({
            'a: loop {
                yield_!(1);
                'a: loop {
                    break 'a;
                }
            }
        });
        assert!(lower(&[], &block).is_ok());
    }

    #[test]
    fn break_outside_of_a_loop_is_rejected() {
        let block: syn::Block = parse_quote!({
            yield_!(1);
            break;
        });
        assert!(error_of(&block).to_string().contains("outside of a loop"));
    }

    #[test]
    fn continue_to_labeled_block_is_rejected() {
        let block: syn::Block = parse_quote!({
            'b: {
                yield_!(1);
                continue 'b;
            }
        });
        assert!(error_of(&block).to_string().contains("labeled block"));
    }

    #[test]
    fn yield_in_foreign_macro_is_rejected() {
        let block: syn::Block = parse_quote!({
            println!("{}", yield_!(1));
        });
        let err = error_of(&block);
        assert!(err.to_string().contains("another macro"));
    }

    #[test]
    fn yield_in_let_else_initializer_is_rejected() {
        let block: syn::Block = parse_quote!({
            let x = yield_!(1) else {
                return;
            };
        });
        assert!(error_of(&block).to_string().contains("initializer of `let ... else`"));
        let block: syn::Block = parse_quote!({
            let Some(x) = f(yield_!(1)) else {
                return;
            };
        });
        assert!(error_of(&block).to_string().contains("initializer of `let ... else`"));
    }

    #[test]
    fn non_simple_resume_binding_is_rejected() {
        let block: syn::Block = parse_quote!({
            let (a, b) = yield_!(1);
        });
        assert!(error_of(&block).to_string().contains("simple identifier"));
    }

    #[test]
    fn yield_with_multiple_arguments_is_rejected() {
        let block: syn::Block = parse_quote!({
            yield_!(1, 2);
        });
        assert!(error_of(&block).to_string().contains("single expression"));
    }

    #[test]
    fn multiple_errors_are_combined() {
        let block: syn::Block = parse_quote!({
            f(yield_!(1));
            g(yield_!(2));
        });
        let err = error_of(&block);
        assert_eq!(err.into_iter().count(), 2);
    }
}
