//! The control-flow graph produced by `lower.rs`: block/terminator data
//! types, the binding table, and post-lowering simplification
//! (goto-chain merging, unreachable-block removal, inlining).

use std::collections::BTreeSet;

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
    /// Index into the defining block's `stmts` of the `let` statement
    /// that introduced this binding, if it was introduced by one (as
    /// opposed to a function argument, a resume binding, or a
    /// match/`for` pattern).
    pub def_stmt: Option<usize>,
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
    /// Bound by a `match`/`while let` arm pattern. There is nowhere to
    /// write a type annotation, so it must not cross a state boundary.
    ArmPat,
    /// Synthetic `__iter{k}` binding holding a `for` loop's iterator.
    ForIter,
    /// Synthetic `__dg{k}` binding holding the coroutine a `yield_all!`
    /// delegates to. Its type follows the operand variable; an
    /// unresolvable type gets a dedicated error message that does not
    /// leak the synthetic name.
    Delegate,
    /// Bound by a destructuring `for` loop pattern. Component types
    /// cannot be derived, so it must not cross a state boundary.
    ForPat,
    /// Bound by a destructuring argument pattern, re-bound by a
    /// synthesized `let` at the top of the entry block. Component types
    /// cannot be derived from the signature, so it must not cross a
    /// state boundary.
    ArgPat,
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

/// Classification of a binding's initializer as a borrow.
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
    /// Successor block ids, in the order a `match`/`if` would evaluate
    /// them. Allocation-free: fixed-arity terminators pack their targets
    /// into a small array, and `Match` walks its arms slice directly.
    pub fn successors(&self) -> Successors<'_> {
        match self {
            Terminator::Goto(b) => Successors::Fixed([Some(*b), None].into_iter()),
            Terminator::Branch { then_, else_, .. } => {
                Successors::Fixed([Some(*then_), Some(*else_)].into_iter())
            }
            Terminator::Match { arms, .. } => Successors::Match(arms.iter()),
            Terminator::Yield { next, .. } => Successors::Fixed([Some(*next), None].into_iter()),
            Terminator::IterNext { body, exit, .. } => {
                Successors::Fixed([Some(*body), Some(*exit)].into_iter())
            }
            Terminator::Return(_) => Successors::Fixed([None, None].into_iter()),
        }
    }
}

/// Iterator over a terminator's successor block ids. `Fixed` covers
/// terminators with at most two successors (the `None` slots are
/// skipped); `Match` has one successor per arm.
pub enum Successors<'a> {
    Fixed(std::array::IntoIter<Option<BlockId>, 2>),
    Match(std::slice::Iter<'a, MatchArm>),
}

impl Iterator for Successors<'_> {
    type Item = BlockId;

    fn next(&mut self) -> Option<BlockId> {
        match self {
            Successors::Fixed(it) => it.find_map(|s| s),
            Successors::Match(it) => it.next().map(|a| a.body),
        }
    }
}

impl DoubleEndedIterator for Successors<'_> {
    fn next_back(&mut self) -> Option<BlockId> {
        match self {
            Successors::Fixed(it) => it.rfind(|s| s.is_some()).flatten(),
            Successors::Match(it) => it.next_back().map(|a| a.body),
        }
    }
}

/// Merges single-predecessor `Goto` chains, drops unreachable blocks,
/// and marks the remaining single-predecessor blocks (branch/match arm
/// targets) as inline. Resume points always stay separate blocks: they
/// are the state-machine's resume entry variants.
pub(crate) fn simplify(cfg: &mut Cfg) {
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
        // c's statements land after b's existing ones, so the def_stmt
        // indices of c's bindings shift by b's statement count.
        let offset = cfg.blocks[b].stmts.len();
        for d in &defs {
            if let Some(i) = &mut cfg.bindings[d.0].def_stmt {
                *i += offset;
            }
        }
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
