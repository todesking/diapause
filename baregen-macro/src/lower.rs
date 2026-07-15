//! Lowers a coroutine body into a control-flow graph (v2 pipeline).
//!
//! Replaces the segment-based IR of `parse.rs` (the switch happens in a
//! later task). Only statements that transitively contain `yield_!` are
//! expanded into CFG structure; every other statement — control flow
//! included — is kept as an opaque statement inside a basic block.

use std::collections::{BTreeSet, HashMap, HashSet};

use syn::spanned::Spanned;
use syn::visit::Visit;

pub type BlockId = usize;

/// Identifies one binding (argument, `let`, resume binding, or pattern
/// binding) uniquely across shadowing and scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BindingId(pub usize);

#[derive(Debug)]
pub struct Binding {
    pub ident: syn::Ident,
    pub mutability: Option<syn::Token![mut]>,
    pub kind: BindingKind,
    /// Syntactic type information; resolved recursively by the analysis.
    pub ty: TySource,
    pub borrow: BorrowSource,
}

/// How a binding was introduced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingKind {
    /// Function argument; its type comes from the signature.
    Arg,
    /// Bound by a `let` statement (simple or destructuring).
    Local,
    /// Resume binding of `let r = yield_!(..);`; its type defaults to
    /// the coroutine's resume type.
    Resume,
    /// Bound by a `match`/`while let` arm pattern. There is no place to
    /// write a type annotation, so it must not cross a state boundary.
    ArmPat,
    /// Synthetic `__iter{k}` binding holding a `for` loop's iterator.
    ForIter,
    /// Bound by a destructuring `for` loop pattern. Component types
    /// cannot be derived, so it must not cross a state boundary.
    ForPat,
}

/// Syntactically determined type of a binding.
// One instance per binding; keeping the syn type inline is simpler than
// boxing it (same trade-off as Terminator).
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum TySource {
    Unknown,
    /// Explicit annotation, literal suffix, or unambiguous literal kind.
    Known(syn::Type),
    /// `let y = x;`: the type follows the moved binding.
    Moved(BindingId),
    /// `a..b` / `a..=b`: `Range<T>` / `RangeInclusive<T>` where `T` is
    /// the first endpoint whose type is known.
    Range {
        inclusive: bool,
        start: Box<TySource>,
        end: Box<TySource>,
    },
    /// A `for` loop's iterator: `<T as IntoIterator>::IntoIter` where
    /// the inner source is the type of the iterated expression.
    IntoIter(Box<TySource>),
    /// A `for` loop's variable: `<I as Iterator>::Item` where `I` is
    /// the type of the loop's `__iter{k}` binding.
    IterItem(BindingId),
}

/// Classification of a binding's initializer as a borrow (ported from
/// v1's analyze.rs).
#[derive(Debug)]
pub enum BorrowSource {
    NotABorrow,
    /// `let y = &x;` / `let y = &mut x;` with a plain identifier source.
    /// `source` is `None` when the identifier is not a local binding
    /// (e.g. a static); the borrow is still rebuilt by name.
    Direct {
        source_ident: syn::Ident,
        source: Option<BindingId>,
        mutable: bool,
    },
    /// A reference that cannot be reconstructed; the message explains why.
    NonReconstructible { why: &'static str },
}

#[derive(Debug)]
pub struct Cfg {
    pub blocks: Vec<Block>,
    pub entry: BlockId,
    /// All bindings, indexed by `BindingId`; the function arguments come
    /// first, in declaration order.
    pub bindings: Vec<Binding>,
}

#[derive(Debug)]
pub struct Block {
    /// Opaque statements: anything that does not contain `yield_!`.
    pub stmts: Vec<syn::Stmt>,
    pub terminator: Terminator,
    /// Bindings read in this block before any local redefinition,
    /// over-approximated for opaque statements.
    pub uses: BTreeSet<BindingId>,
    /// Bindings introduced in this block.
    pub defs: BTreeSet<BindingId>,
    /// Resume entry point after a yield; always becomes an enum variant.
    pub resume_point: bool,
    /// Set by simplification: the block has a unique predecessor and is
    /// emitted inline in that predecessor's transition arm instead of
    /// becoming an enum variant.
    pub inline: bool,
}

// A CFG holds a handful of terminators per coroutine; keeping the syn
// expressions inline is simpler than boxing them.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Terminator {
    /// Unconditional transfer (join points, loop back edges).
    Goto(BlockId),
    /// From `if`; an `if` without `else` points `else_` at the join.
    Branch {
        cond: syn::Expr,
        then_: BlockId,
        else_: BlockId,
    },
    /// From `match` and `while let`.
    Match {
        scrutinee: syn::Expr,
        arms: Vec<MatchArm>,
    },
    /// Suspension point.
    Yield {
        value: syn::Expr,
        resume_binding: Option<ResumeBinding>,
        next: BlockId,
    },
    /// From `for` loops: calls `next()` on the stored iterator and
    /// matches `Some(pat) => body / None => exit`.
    IterNext {
        iter: syn::Ident,
        pat: Box<syn::Pat>,
        body: BlockId,
        exit: BlockId,
    },
    /// End of the coroutine body; the value is the trailing expression
    /// or `()`. Early `return` is handled by expression rewriting, not
    /// by this terminator.
    Return(syn::Expr),
}

#[derive(Debug)]
pub struct MatchArm {
    pub pat: syn::Pat,
    pub guard: Option<syn::Expr>,
    pub body: BlockId,
}

#[derive(Debug)]
pub struct ResumeBinding {
    pub binding: BindingId,
    pub mutability: Option<syn::Token![mut]>,
    pub ty: Option<syn::Type>,
}

impl Terminator {
    pub fn successors(&self) -> Vec<BlockId> {
        match self {
            Terminator::Goto(b) => vec![*b],
            Terminator::Branch { then_, else_, .. } => vec![*then_, *else_],
            Terminator::Match { arms, .. } => arms.iter().map(|a| a.body).collect(),
            Terminator::Yield { next, .. } => vec![*next],
            Terminator::IterNext { body, exit, .. } => vec![*body, *exit],
            Terminator::Return(_) => vec![],
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
    exit: BlockId,
}

struct Lowerer {
    blocks: Vec<DraftBlock>,
    bindings: Vec<Binding>,
    scopes: Vec<HashMap<String, BindingId>>,
    labels: Vec<Frame>,
    errors: Option<syn::Error>,
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
            errors: None,
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
            });
            lw.scopes[0].insert(arg.to_string(), id);
        }
        let entry = lw.new_block(false);
        lw.current = entry;
        lw
    }

    fn lower_fn_body(&mut self, body: &syn::Block) {
        self.scopes.push(HashMap::new());
        self.lower_stmt_list(&body.stmts, TailCtx::FnReturn);
        self.scopes.pop();
    }

    fn finish(self) -> syn::Result<Cfg> {
        if let Some(e) = self.errors {
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
        match &mut self.errors {
            Some(prev) => prev.combine(e),
            None => self.errors = Some(e),
        }
    }

    fn error_count(&self) -> usize {
        self.errors
            .as_ref()
            .map_or(0, |e| e.clone().into_iter().count())
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

    fn resolve(&self, name: &str) -> Option<BindingId> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    /// Introduces a fresh binding into the innermost scope and records
    /// its definition in `block`. Mutability, type, and borrow details
    /// are filled in by the caller where known.
    fn define(&mut self, ident: &syn::Ident, block: BlockId, kind: BindingKind) -> BindingId {
        let id = BindingId(self.bindings.len());
        self.bindings.push(Binding {
            ident: ident.clone(),
            mutability: None,
            kind,
            ty: TySource::Unknown,
            borrow: BorrowSource::NotABorrow,
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
    fn define_pat_bindings(&mut self, pat: &syn::Pat, block: BlockId, kind: BindingKind) {
        let mut c = PatBindingCollector::default();
        c.visit_pat(pat);
        for (ident, mutability) in c.bindings {
            let id = self.define(&ident, block, kind);
            self.bindings[id.0].mutability = mutability;
        }
    }

    /// Rejects any `yield_!` inside `expr` (with `msg`) and any foreign
    /// macro whose tokens mention `yield_!`.
    fn check_no_yield(&mut self, expr: &syn::Expr, msg: &str) {
        let mut checker = YieldBan { msg, error: None };
        checker.visit_expr(expr);
        if let Some(e) = checker.error {
            self.err(e);
        }
    }
}

// === Error messages (ported from v1 where marked) ===

const ERR_STMT_POSITION: &str = "yield_! is only allowed in statement position: \
     `yield_!(expr);` or `let x = yield_!(expr);`";
// v1 wording:
const ERR_TRAILING_YIELD: &str =
    "yield_! as the trailing expression is not supported; add a semicolon";
// v1 wording:
const ERR_FOREIGN_MACRO: &str = "yield_! cannot appear inside another macro invocation";
// v1 wording:
const ERR_LET_ELSE: &str = "`let ... else` cannot be used with yield_!";
// v1 wording:
const ERR_SIMPLE_BINDING: &str =
    "the binding of `let ... = yield_!(...)` must be a simple identifier";
// v1 wording:
const ERR_YIELD_ARG: &str = "yield_! takes a single expression";
const ERR_VALUE_POSITION: &str = "yield_! in value position is not supported; only \
     `yield_!(expr);` and `let x = yield_!(expr);` statements can suspend";
const ERR_TAIL: &str = "yield_! in the trailing expression is not supported; add a semicolon";
const ERR_COND: &str = "yield_! in a condition expression is not supported";
const ERR_SCRUTINEE: &str = "yield_! in a match scrutinee is not supported";
const ERR_GUARD: &str = "yield_! in a match guard is not supported";
const ERR_UNSAFE: &str = "yield_! inside an unsafe block is not supported";
const ERR_IF_LET: &str = "yield_! inside `if let` is not supported; use `match` instead";
const ERR_FOR_HEAD: &str = "yield_! in a `for` loop's iterator expression is not supported";
const ERR_BREAK_VALUE: &str = "`break` with a value cannot target a loop containing yield_!";

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

/// Finds genuine `yield_!` invocations. Closures, async blocks, and
/// nested items are separate scopes and pass through, as in v1. Foreign
/// macros whose tokens mention yield_! do not count as containing a
/// yield; they are rejected separately.
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

    fn visit_expr_closure(&mut self, _: &'ast syn::ExprClosure) {}
    fn visit_expr_async(&mut self, _: &'ast syn::ExprAsync) {}
    fn visit_item(&mut self, _: &'ast syn::Item) {}
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
    error: Option<syn::Error>,
}

impl YieldBan<'_> {
    fn record(&mut self, e: syn::Error) {
        match &mut self.error {
            Some(prev) => prev.combine(e),
            None => self.error = Some(e),
        }
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

    fn visit_expr_closure(&mut self, _: &'ast syn::ExprClosure) {}
    fn visit_expr_async(&mut self, _: &'ast syn::ExprAsync) {}
    fn visit_item(&mut self, _: &'ast syn::Item) {}
}

// === Small collectors ===

/// Collects identifiers that may refer to local variables (ported from
/// v1's analyze.rs). Overapproximates: every unqualified single-segment
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

/// Collects the identifiers a pattern binds, in visit order. The order
/// matches the BindingId assignment order, which the analysis relies on
/// to match `let` statements back to their bindings.
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
    /// this terminates it with `Return`.
    fn lower_stmt_list(&mut self, stmts: &[syn::Stmt], ctx: TailCtx) {
        let n = stmts.len();
        for (i, stmt) in stmts.iter().enumerate() {
            match stmt {
                syn::Stmt::Expr(e, None) if i + 1 == n => self.lower_tail_expr(stmt, e, ctx),
                _ => self.lower_stmt(stmt),
            }
        }
        if ctx == TailCtx::FnReturn && !self.is_current_terminated() {
            self.terminate(Terminator::Return(syn::parse_quote!(())));
        }
    }

    fn lower_tail_expr(&mut self, stmt: &syn::Stmt, e: &syn::Expr, ctx: TailCtx) {
        // A trailing `break`/`continue` transfers control like any other.
        if matches!(e, syn::Expr::Break(_) | syn::Expr::Continue(_)) {
            return self.lower_stmt(stmt);
        }
        // A trailing loop always evaluates to `()` (a `break` with a
        // value is rejected separately), so it is safe to lower as a
        // statement even as the tail expression.
        if matches!(
            e,
            syn::Expr::Loop(_) | syn::Expr::While(_) | syn::Expr::ForLoop(_)
        ) && contains_yield_expr(e)
        {
            return self.lower_stmt(stmt);
        }
        match ctx {
            TailCtx::FnReturn => {
                if contains_yield_expr(e) {
                    // The trailing expression is the return value, so a
                    // yield inside it is value-position yield. Lower the
                    // statement anyway to surface the most specific
                    // error; if it lowers cleanly the only problem is
                    // its tail position.
                    let before = self.error_count();
                    self.lower_stmt(stmt);
                    if self.error_count() == before {
                        self.err(syn::Error::new_spanned(e, ERR_TAIL));
                    }
                } else {
                    self.check_no_yield(e, ERR_TAIL); // foreign macro scan
                    self.record_expr_uses(e, self.current);
                    self.terminate(Terminator::Return(e.clone()));
                }
            }
            TailCtx::Discard => {
                if is_block_like(e) || contains_yield_expr(e) {
                    self.lower_stmt(stmt);
                } else {
                    // The value is discarded, so a semicolon keeps the
                    // statement sequence valid when more code follows in
                    // the same generated arm.
                    let wrapped = syn::Stmt::Expr(e.clone(), Some(Default::default()));
                    self.push_opaque(&wrapped);
                }
            }
        }
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
            syn::Stmt::Local(local) => {
                self.err(syn::Error::new_spanned(local, ERR_VALUE_POSITION));
            }
            syn::Stmt::Expr(e, _) => self.lower_control_expr(e),
            // Stmt::Macro with yield tokens is caught as opaque; items
            // never contain our yield.
            _ => self.push_opaque(stmt),
        }
    }

    fn lower_let_yield(&mut self, local: &syn::Local) {
        let init = local.init.as_ref().expect("BUG: checked by caller");
        if let Some((else_token, _)) = &init.diverge {
            return self.err(syn::Error::new_spanned(else_token, ERR_LET_ELSE));
        }
        let syn::Expr::Macro(m) = &*init.expr else {
            unreachable!()
        };
        let (pat, ty) = match &local.pat {
            syn::Pat::Type(pt) => (&*pt.pat, Some((*pt.ty).clone())),
            other => (other, None),
        };
        let binding = match pat {
            syn::Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => {
                (pi.ident.clone(), pi.mutability, ty)
            }
            other => {
                return self.err(syn::Error::new_spanned(other, ERR_SIMPLE_BINDING));
            }
        };
        self.lower_yield(&m.mac, Some(binding));
    }

    /// Ends the current block with a `Yield` terminator and switches to
    /// the resume-point continuation block.
    fn lower_yield(
        &mut self,
        mac: &syn::Macro,
        binding: Option<(syn::Ident, Option<syn::Token![mut]>, Option<syn::Type>)>,
    ) {
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
        let resume_binding = binding.map(|(ident, mutability, ty)| {
            let id = self.define(&ident, next, BindingKind::Resume);
            let b = &mut self.bindings[id.0];
            b.mutability = mutability;
            if let Some(t) = &ty {
                b.ty = TySource::Known(t.clone());
            }
            ResumeBinding {
                binding: id,
                mutability,
                ty,
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
        let Some((_, exit)) = frame else {
            return self.err(syn::Error::new_spanned(
                b,
                match &b.label {
                    Some(l) => format!("use of undeclared label `{l}`"),
                    None => "`break` outside of a loop".to_string(),
                },
            ));
        };
        if let Some(value) = &b.expr {
            self.err(syn::Error::new_spanned(value, ERR_BREAK_VALUE));
        }
        self.terminate(Terminator::Goto(exit));
        // Anything after the jump is unreachable; lower it into a fresh
        // block that simplification will drop.
        self.current = self.new_block(false);
    }

    fn lower_continue(&mut self, c: &syn::ExprContinue) {
        let frame = match &c.label {
            Some(l) => self.find_labeled_frame(&l.ident.to_string()),
            None => self.innermost_loop_frame(),
        };
        let Some((header, _)) = frame else {
            return self.err(syn::Error::new_spanned(
                c,
                match &c.label {
                    Some(l) => format!("use of undeclared label `{l}`"),
                    None => "`continue` outside of a loop".to_string(),
                },
            ));
        };
        let Some(header) = header else {
            return self.err(syn::Error::new_spanned(
                c,
                "`continue` cannot target a labeled block",
            ));
        };
        self.terminate(Terminator::Goto(header));
        self.current = self.new_block(false);
    }

    fn find_labeled_frame(&self, name: &str) -> Option<(Option<BlockId>, BlockId)> {
        self.labels
            .iter()
            .rev()
            .find(|f| f.label.as_deref() == Some(name))
            .map(|f| (f.header, f.exit))
    }

    fn innermost_loop_frame(&self) -> Option<(Option<BlockId>, BlockId)> {
        self.labels
            .iter()
            .rev()
            .find(|f| f.header.is_some())
            .map(|f| (f.header, f.exit))
    }

    /// Appends a statement without yield_! to the current block.
    fn push_opaque(&mut self, stmt: &syn::Stmt) {
        self.validate_opaque(stmt);
        self.record_stmt_uses(stmt);
        if let syn::Stmt::Local(local) = stmt {
            self.define_local(local);
        }
        self.blocks[self.current].stmts.push(stmt.clone());
    }

    /// Introduces the bindings of an opaque `let`, classifying the type
    /// source and borrow kind of a simple-identifier binding (ported
    /// from v1's collect_let_defs). Classification runs before the
    /// bindings enter scope, so the initializer resolves against the
    /// enclosing environment (`let x = x;` sees the outer `x`).
    fn define_local(&mut self, local: &syn::Local) {
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
                let id = self.define(&pi.ident, self.current, BindingKind::Local);
                let b = &mut self.bindings[id.0];
                b.mutability = pi.mutability;
                b.ty = ty;
                b.borrow = borrow;
            }
            // Destructuring patterns: every bound identifier becomes a
            // binding of unknown type (as in v1).
            other => self.define_pat_bindings(other, self.current, BindingKind::Local),
        }
    }

    /// Syntactic type inference for an initializer expression.
    fn infer_ty_source(&self, expr: &syn::Expr) -> TySource {
        match expr {
            syn::Expr::Paren(p) => self.infer_ty_source(&p.expr),
            // Negation of a suffixed numeric literal keeps its type.
            syn::Expr::Unary(u) if matches!(u.op, syn::UnOp::Neg(_)) => match &*u.expr {
                syn::Expr::Lit(_) => self.infer_ty_source(&u.expr),
                _ => TySource::Unknown,
            },
            syn::Expr::Lit(lit) => infer_lit_ty(&lit.lit).map_or(TySource::Unknown, TySource::Known),
            // Move propagation: `let y = x;` follows x's type.
            syn::Expr::Path(p) if p.qself.is_none() => p
                .path
                .get_ident()
                .and_then(|i| self.resolve(&i.to_string()))
                .map_or(TySource::Unknown, TySource::Moved),
            syn::Expr::Range(r) => match (&r.start, &r.end) {
                (Some(start), Some(end)) => TySource::Range {
                    inclusive: matches!(r.limits, syn::RangeLimits::Closed(_)),
                    start: Box::new(self.infer_ty_source(start)),
                    end: Box::new(self.infer_ty_source(end)),
                },
                _ => TySource::Unknown,
            },
            _ => TySource::Unknown,
        }
    }

    fn classify_borrow(&self, init: Option<&syn::Expr>, annotated: Option<&syn::Type>) -> BorrowSource {
        let mut init = init;
        while let Some(syn::Expr::Paren(p)) = init {
            init = Some(&p.expr);
        }
        match init {
            Some(syn::Expr::Reference(r)) => match &*r.expr {
                syn::Expr::Path(p) if p.qself.is_none() && p.path.get_ident().is_some() => {
                    let source_ident = p.path.get_ident().unwrap().clone();
                    BorrowSource::Direct {
                        source: self.resolve(&source_ident.to_string()),
                        source_ident,
                        mutable: r.mutability.is_some(),
                    }
                }
                _ => BorrowSource::NonReconstructible {
                    why: "is held across yield_! but borrows a non-trivial place; only direct \
                          borrows of local variables (`let y = &x;` / `let y = &mut x;`) can be \
                          reconstructed after resume",
                },
            },
            _ if matches!(annotated, Some(syn::Type::Reference(_))) => {
                BorrowSource::NonReconstructible {
                    why: "has a reference type but is not a direct borrow (`let y = &x;` / \
                          `let y = &mut x;`), so it cannot be held across yield_!",
                }
            }
            _ => BorrowSource::NotABorrow,
        }
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

// === Control-flow expansion (statements that contain yield_!) ===

impl Lowerer {
    fn lower_control_expr(&mut self, e: &syn::Expr) {
        match e {
            syn::Expr::If(ei) => self.lower_if(ei),
            syn::Expr::Match(em) => self.lower_match(em),
            syn::Expr::Loop(el) => self.lower_loop(el),
            syn::Expr::While(ew) => self.lower_while(ew),
            syn::Expr::ForLoop(ef) => self.lower_for(ef),
            syn::Expr::Block(eb) => self.lower_block_stmt(eb),
            syn::Expr::Unsafe(eu) => {
                self.err(syn::Error::new_spanned(eu.unsafe_token, ERR_UNSAFE));
            }
            other => self.err(syn::Error::new_spanned(other, ERR_STMT_POSITION)),
        }
    }

    fn lower_if(&mut self, ei: &syn::ExprIf) {
        let join = self.new_block(false);
        self.lower_if_arms(ei, join);
        self.current = join;
    }

    /// Lowers one `if`/`else if` link of a chain; every arm exits to the
    /// shared `join` block.
    fn lower_if_arms(&mut self, ei: &syn::ExprIf, join: BlockId) {
        if matches!(&*ei.cond, syn::Expr::Let(_)) {
            self.err(syn::Error::new_spanned(ei.if_token, ERR_IF_LET));
            self.terminate(Terminator::Goto(join));
            return;
        }
        self.check_no_yield(&ei.cond, ERR_COND);
        self.record_expr_uses(&ei.cond, self.current);
        let then_bb = self.new_block(false);
        let else_bb = match &ei.else_branch {
            None => join,
            Some(_) => self.new_block(false),
        };
        self.terminate(Terminator::Branch {
            cond: (*ei.cond).clone(),
            then_: then_bb,
            else_: else_bb,
        });
        self.current = then_bb;
        self.scopes.push(HashMap::new());
        self.lower_stmt_list(&ei.then_branch.stmts, TailCtx::Discard);
        self.scopes.pop();
        self.terminate(Terminator::Goto(join));
        if let Some((_, else_expr)) = &ei.else_branch {
            self.current = else_bb;
            match &**else_expr {
                syn::Expr::Block(b) => {
                    self.scopes.push(HashMap::new());
                    self.lower_stmt_list(&b.block.stmts, TailCtx::Discard);
                    self.scopes.pop();
                    self.terminate(Terminator::Goto(join));
                }
                syn::Expr::If(nested) => self.lower_if_arms(nested, join),
                _ => unreachable!("else branch is always a block or an if"),
            }
        }
    }

    fn lower_match(&mut self, em: &syn::ExprMatch) {
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
            self.scopes.push(HashMap::new());
            self.define_pat_bindings(&arm.pat, body_bb, BindingKind::ArmPat);
            let body_stmt = wrap_arm_body(&arm.body);
            self.lower_stmt_list(std::slice::from_ref(&body_stmt), TailCtx::Discard);
            self.scopes.pop();
            self.terminate(Terminator::Goto(join));
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

    fn lower_loop(&mut self, el: &syn::ExprLoop) {
        let header = self.new_block(false);
        self.terminate(Terminator::Goto(header));
        let exit = self.new_block(false);
        self.labels.push(Frame {
            label: el.label.as_ref().map(|l| l.name.ident.to_string()),
            header: Some(header),
            exit,
        });
        self.current = header;
        self.scopes.push(HashMap::new());
        self.lower_stmt_list(&el.body.stmts, TailCtx::Discard);
        self.scopes.pop();
        self.terminate(Terminator::Goto(header));
        self.labels.pop();
        self.current = exit;
    }

    fn lower_while(&mut self, ew: &syn::ExprWhile) {
        if let syn::Expr::Let(el) = &*ew.cond {
            return self.lower_while_let(ew, el);
        }
        let header = self.new_block(false);
        self.terminate(Terminator::Goto(header));
        self.current = header;
        self.check_no_yield(&ew.cond, ERR_COND);
        self.record_expr_uses(&ew.cond, header);
        let body = self.new_block(false);
        let exit = self.new_block(false);
        self.terminate(Terminator::Branch {
            cond: (*ew.cond).clone(),
            then_: body,
            else_: exit,
        });
        self.labels.push(Frame {
            label: ew.label.as_ref().map(|l| l.name.ident.to_string()),
            header: Some(header),
            exit,
        });
        self.current = body;
        self.scopes.push(HashMap::new());
        self.lower_stmt_list(&ew.body.stmts, TailCtx::Discard);
        self.scopes.pop();
        self.terminate(Terminator::Goto(header));
        self.labels.pop();
        self.current = exit;
    }

    fn lower_while_let(&mut self, ew: &syn::ExprWhile, el: &syn::ExprLet) {
        let header = self.new_block(false);
        self.terminate(Terminator::Goto(header));
        self.current = header;
        self.check_no_yield(&el.expr, ERR_SCRUTINEE);
        self.record_expr_uses(&el.expr, header);
        let body = self.new_block(false);
        let exit = self.new_block(false);
        self.set_terminator(
            header,
            Terminator::Match {
                scrutinee: (*el.expr).clone(),
                arms: vec![
                    MatchArm {
                        pat: (*el.pat).clone(),
                        guard: None,
                        body,
                    },
                    MatchArm {
                        pat: syn::parse_quote!(_),
                        guard: None,
                        body: exit,
                    },
                ],
            },
        );
        self.labels.push(Frame {
            label: ew.label.as_ref().map(|l| l.name.ident.to_string()),
            header: Some(header),
            exit,
        });
        self.current = body;
        self.scopes.push(HashMap::new());
        self.define_pat_bindings(&el.pat, body, BindingKind::ArmPat);
        self.lower_stmt_list(&ew.body.stmts, TailCtx::Discard);
        self.scopes.pop();
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
        let iter_id = self.define(&iter_ident, self.current, BindingKind::ForIter);
        let b = &mut self.bindings[iter_id.0];
        b.mutability = Some(syn::Token![mut](ef.expr.span()));
        b.ty = TySource::IntoIter(Box::new(head_ty));

        let header = self.new_block(false);
        self.terminate(Terminator::Goto(header));
        let body = self.new_block(false);
        let exit = self.new_block(false);
        // The `next()` call consumes the iterator at the header.
        self.blocks[header].uses.insert(iter_id);
        self.set_terminator(
            header,
            Terminator::IterNext {
                iter: iter_ident,
                pat: Box::new((*ef.pat).clone()),
                body,
                exit,
            },
        );
        self.labels.push(Frame {
            label: ef.label.as_ref().map(|l| l.name.ident.to_string()),
            header: Some(header),
            exit,
        });
        self.current = body;
        self.scopes.push(HashMap::new());
        match &*ef.pat {
            // A simple-identifier loop variable gets the iterator's item
            // type; destructured components have no derivable type and
            // must not cross a state boundary.
            syn::Pat::Ident(pi) if pi.by_ref.is_none() && pi.subpat.is_none() => {
                let id = self.define(&pi.ident, body, BindingKind::Local);
                let b = &mut self.bindings[id.0];
                b.mutability = pi.mutability;
                b.ty = TySource::IterItem(iter_id);
            }
            other => self.define_pat_bindings(other, body, BindingKind::ForPat),
        }
        self.lower_stmt_list(&ef.body.stmts, TailCtx::Discard);
        self.scopes.pop();
        self.terminate(Terminator::Goto(header));
        self.labels.pop();
        self.current = exit;
    }

    /// Rejects `for x in &local` / `for x in &mut local` where `local`
    /// is a body-local binding: the stored iterator would borrow another
    /// field of the same state. Borrows of arguments point outside the
    /// state and are fine; method calls (`local.iter()`) are left to
    /// borrowck.
    fn check_for_local_borrow(&mut self, expr: &syn::Expr) {
        let mut e = expr;
        while let syn::Expr::Paren(p) = e {
            e = &p.expr;
        }
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

    /// A block statement: the contents are lowered in place. A labeled
    /// block additionally becomes a `break` target.
    fn lower_block_stmt(&mut self, eb: &syn::ExprBlock) {
        match &eb.label {
            Some(label) => {
                let join = self.new_block(false);
                self.labels.push(Frame {
                    label: Some(label.name.ident.to_string()),
                    header: None,
                    exit: join,
                });
                self.scopes.push(HashMap::new());
                self.lower_stmt_list(&eb.block.stmts, TailCtx::Discard);
                self.scopes.pop();
                self.terminate(Terminator::Goto(join));
                self.labels.pop();
                self.current = join;
            }
            None => {
                self.scopes.push(HashMap::new());
                self.lower_stmt_list(&eb.block.stmts, TailCtx::Discard);
                self.scopes.pop();
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
            error: None,
        };
        checker.visit_stmt(stmt);
        if let Some(e) = checker.error {
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
    error: Option<syn::Error>,
}

impl OpaqueChecker {
    fn record(&mut self, e: syn::Error) {
        match &mut self.error {
            Some(prev) => prev.combine(e),
            None => self.error = Some(e),
        }
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
    // them, not to the coroutine (same pass-through as v1).
    fn visit_expr_closure(&mut self, _: &'ast syn::ExprClosure) {}
    fn visit_expr_async(&mut self, _: &'ast syn::ExprAsync) {}
    fn visit_item(&mut self, _: &'ast syn::Item) {}
}

// === CFG simplification ===

/// Merges single-predecessor `Goto` chains, drops unreachable blocks,
/// and marks the remaining single-predecessor blocks (branch/match arm
/// targets) as inline. Resume points always stay separate blocks: they
/// are the state-machine's resume entry variants.
fn simplify(cfg: &mut Cfg) {
    remove_unreachable(cfg);
    merge_goto_chains(cfg);
    remove_unreachable(cfg);
    let preds = pred_edge_counts(&cfg.blocks);
    for (i, b) in cfg.blocks.iter_mut().enumerate() {
        b.inline = i != cfg.entry && !b.resume_point && preds[i] == 1;
    }
}

fn pred_edge_counts(blocks: &[Block]) -> Vec<usize> {
    let mut counts = vec![0usize; blocks.len()];
    for b in blocks {
        for s in b.terminator.successors() {
            counts[s] += 1;
        }
    }
    counts
}

fn merge_goto_chains(cfg: &mut Cfg) {
    loop {
        let preds = pred_edge_counts(&cfg.blocks);
        let mut pair = None;
        for (b, blk) in cfg.blocks.iter().enumerate() {
            if let Terminator::Goto(c) = &blk.terminator {
                let c = *c;
                if c != b && c != cfg.entry && preds[c] == 1 && !cfg.blocks[c].resume_point {
                    pair = Some((b, c));
                    break;
                }
            }
        }
        let Some((b, c)) = pair else { break };
        // Absorb c into b; c becomes an unreachable tombstone that the
        // caller removes afterwards.
        let mut stmts = std::mem::take(&mut cfg.blocks[c].stmts);
        let term = std::mem::replace(
            &mut cfg.blocks[c].terminator,
            Terminator::Return(syn::parse_quote!(())),
        );
        let uses = std::mem::take(&mut cfg.blocks[c].uses);
        let defs = std::mem::take(&mut cfg.blocks[c].defs);
        let bb = &mut cfg.blocks[b];
        bb.stmts.append(&mut stmts);
        bb.terminator = term;
        for u in uses {
            if !bb.defs.contains(&u) {
                bb.uses.insert(u);
            }
        }
        bb.defs.extend(defs);
    }
}

fn remove_unreachable(cfg: &mut Cfg) {
    let mut reachable = vec![false; cfg.blocks.len()];
    let mut stack = vec![cfg.entry];
    while let Some(b) = stack.pop() {
        if std::mem::replace(&mut reachable[b], true) {
            continue;
        }
        stack.extend(cfg.blocks[b].terminator.successors());
    }
    if reachable.iter().all(|r| *r) {
        return;
    }
    let mut remap = vec![usize::MAX; cfg.blocks.len()];
    let mut next = 0;
    for (i, r) in reachable.iter().enumerate() {
        if *r {
            remap[i] = next;
            next += 1;
        }
    }
    let blocks = std::mem::take(&mut cfg.blocks);
    cfg.blocks = blocks
        .into_iter()
        .enumerate()
        .filter(|(i, _)| reachable[*i])
        .map(|(_, b)| b)
        .collect();
    for b in &mut cfg.blocks {
        retarget(&mut b.terminator, &remap);
    }
    cfg.entry = remap[cfg.entry];
}

fn retarget(t: &mut Terminator, remap: &[usize]) {
    match t {
        Terminator::Goto(b) => *b = remap[*b],
        Terminator::Branch { then_, else_, .. } => {
            *then_ = remap[*then_];
            *else_ = remap[*else_];
        }
        Terminator::Match { arms, .. } => {
            for arm in arms {
                arm.body = remap[arm.body];
            }
        }
        Terminator::Yield { next, .. } => *next = remap[*next],
        Terminator::IterNext { body, exit, .. } => {
            *body = remap[*body];
            *exit = remap[*exit];
        }
        Terminator::Return(_) => {}
    }
}

/// Turns a match arm body into a statement for lowering: blocks keep
/// their statements, other expressions get a semicolon since the arm's
/// value is discarded in statement-position matches.
fn wrap_arm_body(body: &syn::Expr) -> syn::Stmt {
    syn::Stmt::Expr(
        body.clone(),
        if is_block_like(body) {
            None
        } else {
            Some(Default::default())
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    fn lower_args(args: &[&str], block: &syn::Block) -> Cfg {
        let idents: Vec<syn::Ident> = args
            .iter()
            .map(|a| syn::Ident::new(a, proc_macro2::Span::call_site()))
            .collect();
        lower(&idents, block).unwrap()
    }

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

    // === Straight-line code (v1 parity) ===

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
    fn straight_line_yields_match_v1_segments() {
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
        let block: syn::Block = parse_quote!({
            let x: u32 = if c {
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
    fn yield_in_trailing_expression_is_rejected() {
        let block: syn::Block = parse_quote!({
            if c {
                yield_!(1);
            }
        });
        assert!(error_of(&block).to_string().contains("add a semicolon"));
        // Bare trailing yield keeps the v1 message.
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
    fn yield_in_if_let_is_rejected() {
        let block: syn::Block = parse_quote!({
            if let Some(x) = opt {
                yield_!(x);
            }
        });
        assert!(error_of(&block).to_string().contains("if let"));
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
    fn break_with_value_is_rejected() {
        let block: syn::Block = parse_quote!({
            let x: u32 = loop {
                yield_!(1);
                break 5;
            };
        });
        // The whole `let` is value-position; a direct loop hits the
        // break-value error.
        assert!(error_of(&block).to_string().contains("value"));
        let block: syn::Block = parse_quote!({
            loop {
                yield_!(1);
                break 5;
            }
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
    fn let_else_with_yield_is_rejected() {
        let block: syn::Block = parse_quote!({
            let x = yield_!(1) else {
                return;
            };
        });
        assert!(error_of(&block).to_string().contains("let ... else"));
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
