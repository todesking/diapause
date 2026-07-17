//! CFG-based liveness analysis, type determination, and borrow
//! reconstruction. Consumes the CFG built by `lower.rs`.
//!
//! Every block that is not inlined into its predecessor's arm becomes a
//! state-enum variant; its fields are the bindings live at its entry.
//! Direct borrows never enter a variant: their uses are attributed to
//! the borrowed binding instead, and the borrow is re-established at the
//! head of every arm (region) that uses it outside its defining region.

use std::collections::{BTreeMap, BTreeSet};

use quote::format_ident;
use syn::visit::Visit;

use crate::cfg::{
    BindingId, BindingKind, BlockId, BorrowSource, Cfg, OpaqueJumpKind, Terminator, TySource,
};
use crate::lower::{ErrorSink, UseCollector};

/// Signature-level information about one function argument, parallel to
/// `BindingId(0)..BindingId(args.len())`. Built from `expand::ArgVar` (via
/// its `From` impl), minus the `ident`, since arguments are identified by
/// position here rather than by name.
#[derive(Debug)]
pub struct ArgInfo {
    pub mutability: Option<syn::Token![mut]>,
    pub ty: syn::Type,
}

/// A field of a state variant.
#[derive(Debug)]
pub struct StateField {
    pub ident: syn::Ident,
    pub mutability: Option<syn::Token![mut]>,
    pub ty: syn::Type,
}

/// A direct borrow to re-establish at the head of a block's arm, before
/// its statements run.
#[derive(Debug)]
pub struct Reborrow {
    pub target: syn::Ident,
    pub target_mut: Option<syn::Token![mut]>,
    pub source: syn::Ident,
    pub mutable: bool,
}

/// A block that becomes a coroutine state (an enum variant): a non-inline
/// block, its name, the fields holding its live-in bindings (after borrow
/// substitution), and the borrows to rebuild at the head of its arm, in
/// definition order (chains rebuild sources before their dependents).
#[derive(Debug)]
pub struct Variant {
    pub block: BlockId,
    pub ident: syn::Ident,
    pub fields: Vec<StateField>,
    pub reborrows: Vec<Reborrow>,
}

#[derive(Debug)]
pub struct Analysis {
    /// One entry per non-inline block (including the entry block), in
    /// ascending `BlockId` order.
    pub variants: Vec<Variant>,
    /// Indexed by `BlockId`: statement indices to omit from codegen
    /// (original borrow `let`s whose binding is only used in other arms).
    pub removed_stmts: Vec<BTreeSet<usize>>,
    /// Indexed by `BlockId`: bindings live at block entry, after borrow
    /// substitution.
    // Consumed by the unit tests; codegen uses `variants`.
    #[allow(dead_code)]
    pub live_in: Vec<BTreeSet<BindingId>>,
}

impl Analysis {
    /// The variant for `block`, or `None` if `block` is inlined into its
    /// predecessor's arm.
    pub fn variant(&self, block: BlockId) -> Option<&Variant> {
        self.variants
            .binary_search_by_key(&block, |v| v.block)
            .ok()
            .map(|i| &self.variants[i])
    }
}

/// Analyzes a lowered coroutine CFG. `args` describes the function
/// arguments (`BindingId(0)..`); `resume_ty` is the default type of
/// resume bindings.
pub fn analyze(cfg: &Cfg, args: &[ArgInfo], resume_ty: &syn::Type) -> syn::Result<Analysis> {
    let cx = Context::new(cfg, args, resume_ty);
    cx.run()
}

struct Context<'a> {
    cfg: &'a Cfg,
    args: &'a [ArgInfo],
    resume_ty: &'a syn::Type,
    /// The root of each block's region: the non-inline block whose arm
    /// textually contains it.
    region: Vec<BlockId>,
    /// The block whose entry defines each binding; `None` for arguments
    /// (in scope from entry) and bindings in removed unreachable blocks.
    def_block: Vec<Option<BlockId>>,
    errors: ErrorSink,
}

impl<'a> Context<'a> {
    fn new(cfg: &'a Cfg, args: &'a [ArgInfo], resume_ty: &'a syn::Type) -> Self {
        Context {
            cfg,
            args,
            resume_ty,
            region: region_roots(cfg),
            def_block: def_blocks(cfg),
            errors: ErrorSink::default(),
        }
    }

    fn err(&mut self, span: proc_macro2::Span, msg: String) {
        self.errors.push(syn::Error::new(span, msg));
    }

    fn run(mut self) -> syn::Result<Analysis> {
        let (uses, rebuilds) = self.substitute_borrows();
        let live_in = self.liveness(&uses);
        let removed_stmts = self.removed_statements(&rebuilds);
        let variants = self.build_variants(&live_in, &rebuilds);
        self.check_jump_shadowing(&variants);
        self.errors.into_result(Analysis {
            variants,
            removed_stmts,
            live_in,
        })
    }
}

// === Borrow substitution and liveness ===

/// Applies `step` to every index yielded by `order`, repeating full passes
/// until a pass leaves every index unchanged. `step` reports per-index
/// whether it changed anything; `order` is re-walked from the start on
/// each pass, so its iteration sequence is part of the fixed point being
/// computed and must be chosen deliberately by callers.
fn fixpoint<I>(order: I, mut step: impl FnMut(usize) -> bool)
where
    I: IntoIterator<Item = usize> + Clone,
{
    loop {
        let mut changed = false;
        for i in order.clone() {
            if step(i) {
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

impl Context<'_> {
    /// Rewrites the per-block use sets so that a use of a direct-borrow
    /// binding outside its defining region counts as a use of the
    /// borrowed binding, and records the borrow for rebuilding at the
    /// using region's root. Iterates to a fixed point to resolve borrow
    /// chains (`let y = &x; let z = &y;`).
    fn substitute_borrows(&self) -> (Vec<BTreeSet<BindingId>>, Vec<BTreeSet<BindingId>>) {
        let mut uses: Vec<BTreeSet<BindingId>> =
            self.cfg.blocks.iter().map(|b| b.uses.clone()).collect();
        let mut rebuilds: Vec<BTreeSet<BindingId>> = vec![BTreeSet::new(); self.cfg.blocks.len()];
        fixpoint(0..self.cfg.blocks.len(), |b| {
            let root = self.region[b];
            let foreign: Vec<BindingId> = uses[b]
                .iter()
                .copied()
                .filter(|id| self.is_foreign_borrow(*id, root))
                .collect();
            let mut changed = false;
            for t in foreign {
                uses[b].remove(&t);
                rebuilds[root].insert(t);
                let BorrowSource::Direct { source, .. } = &self.cfg.bindings[t.0].borrow else {
                    unreachable!("BUG: is_foreign_borrow admits only direct borrows")
                };
                if let Some(s) = source {
                    uses[b].insert(*s);
                }
                changed = true;
            }
            changed
        });
        (uses, rebuilds)
    }

    /// A direct borrow defined in a different region than `root`: its
    /// original `let` is not in scope in `root`'s arm.
    fn is_foreign_borrow(&self, id: BindingId, root: BlockId) -> bool {
        matches!(self.cfg.bindings[id.0].borrow, BorrowSource::Direct { .. })
            && matches!(self.def_block[id.0], Some(d) if self.region[d] != root)
    }

    /// Standard backward dataflow to a fixed point (the CFG has back
    /// edges): `live_in(B) = use(B) ∪ (∪ live_in(succ) ∖ def(B))`.
    /// Successors include opaque-jump edges: a jump can fire anywhere in
    /// the block, so its target's live-ins are (conservatively) live
    /// through the whole block.
    fn liveness(&self, uses: &[BTreeSet<BindingId>]) -> Vec<BTreeSet<BindingId>> {
        let n = self.cfg.blocks.len();
        let mut live_in: Vec<BTreeSet<BindingId>> = vec![BTreeSet::new(); n];
        fixpoint((0..n).rev(), |b| {
            let mut set = BTreeSet::new();
            for s in self.cfg.successors(b) {
                set.extend(live_in[s].iter().copied());
            }
            for d in &self.cfg.blocks[b].defs {
                set.remove(d);
            }
            set.extend(uses[b].iter().copied());
            if set != live_in[b] {
                live_in[b] = set;
                true
            } else {
                false
            }
        });
        live_in
    }

    /// Original borrow `let`s to omit: a rebuilt borrow's defining
    /// statement is dropped unless the binding is still used within its
    /// own region after the definition (borrows have no side effects).
    fn removed_statements(&self, rebuilds: &[BTreeSet<BindingId>]) -> Vec<BTreeSet<usize>> {
        let mut out: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); self.cfg.blocks.len()];
        let crossed: BTreeSet<BindingId> = rebuilds.iter().flatten().copied().collect();
        for t in crossed {
            let (Some(d), Some(i)) = (self.def_block[t.0], self.cfg.bindings[t.0].def_stmt) else {
                continue;
            };
            let name = self.cfg.bindings[t.0].ident.to_string();
            // Scan the rest of the defining block by name (conservative:
            // a same-named use keeps the statement, which is harmless).
            let mut c = UseCollector::default();
            for stmt in &self.cfg.blocks[d].stmts[i + 1..] {
                c.visit_stmt(stmt);
            }
            for e in terminator_exprs(&self.cfg.blocks[d].terminator) {
                c.visit_expr(e);
            }
            let used_later = c.used.contains(&name)
                || self.cfg.blocks.iter().enumerate().any(|(b, blk)| {
                    b != d && self.region[b] == self.region[d] && blk.uses.contains(&t)
                });
            if !used_later {
                out[d].insert(i);
            }
        }
        out
    }

    /// Reborrows to rebuild for one block's rebuilt-binding set.
    fn build_reborrows_for(&self, ids: &BTreeSet<BindingId>) -> Vec<Reborrow> {
        ids.iter()
            .map(|t| {
                let b = &self.cfg.bindings[t.0];
                let BorrowSource::Direct {
                    source_ident,
                    mutable,
                    ..
                } = &b.borrow
                else {
                    unreachable!("BUG: rebuild of a non-borrow binding")
                };
                Reborrow {
                    target: b.ident.clone(),
                    target_mut: b.mutability,
                    source: source_ident.clone(),
                    mutable: *mutable,
                }
            })
            .collect()
    }
}

// === Variant fields, type determination, and errors ===

impl Context<'_> {
    /// Builds the `Variant` list: one entry per non-inline block, in
    /// ascending `BlockId` order, combining its fields, its reborrows, and
    /// the name assigned by `variant_idents`.
    fn build_variants(
        &mut self,
        live_in: &[BTreeSet<BindingId>],
        rebuilds: &[BTreeSet<BindingId>],
    ) -> Vec<Variant> {
        let forced_mut = self.compute_forced_mut(rebuilds);
        let mut reported: BTreeSet<BindingId> = BTreeSet::new();
        let mut collisions: BTreeSet<(BindingId, BindingId)> = BTreeSet::new();
        let idents = variant_idents(self.cfg);
        let mut variants = Vec::new();
        for b in 0..self.cfg.blocks.len() {
            if self.cfg.blocks[b].inline {
                continue;
            }
            let fields = self.build_fields_for_block(
                &live_in[b],
                &forced_mut,
                &mut reported,
                &mut collisions,
            );
            let reborrows = self.build_reborrows_for(&rebuilds[b]);
            let ident = idents
                .get(&b)
                .cloned()
                .expect("BUG: every non-inline block must have a variant name");
            variants.push(Variant {
                block: b,
                ident,
                fields,
                reborrows,
            });
        }
        variants
    }

    /// Bindings whose mutable borrow is rebuilt in some arm must be
    /// unpacked `mut` so the reborrow can take `&mut`.
    fn compute_forced_mut(&self, rebuilds: &[BTreeSet<BindingId>]) -> BTreeSet<BindingId> {
        rebuilds
            .iter()
            .flatten()
            .filter_map(|t| match &self.cfg.bindings[t.0].borrow {
                BorrowSource::Direct {
                    source: Some(s),
                    mutable: true,
                    ..
                } => Some(*s),
                _ => None,
            })
            .collect()
    }

    /// Builds the state fields for one non-inline block's live-in set,
    /// reporting a name collision or a per-binding error (at most once
    /// per binding) in place of a field where the binding can't become
    /// one.
    fn build_fields_for_block(
        &mut self,
        live: &BTreeSet<BindingId>,
        forced_mut: &BTreeSet<BindingId>,
        reported: &mut BTreeSet<BindingId>,
        collisions: &mut BTreeSet<(BindingId, BindingId)>,
    ) -> Vec<StateField> {
        self.check_name_collisions(live, collisions);
        let mut fields = Vec::new();
        for id in live {
            let binding = &self.cfg.bindings[id.0];
            let name = binding.ident.to_string();
            match &binding.borrow {
                // Substituted by its source during liveness.
                BorrowSource::Direct { .. } => {
                    debug_assert!(
                        self.def_block[id.0].is_none(),
                        "BUG: direct borrow live at a variant entry"
                    );
                    continue;
                }
                BorrowSource::NonReconstructible { why } => {
                    if reported.insert(*id) {
                        self.err(binding.ident.span(), format!("`{name}` {why}"));
                    }
                    continue;
                }
                BorrowSource::NotABorrow => {}
            }
            let unannotatable = match binding.kind {
                BindingKind::ArmPat => Some((
                    "a match arm pattern",
                    "arm patterns cannot be annotated with a type",
                    "the arm",
                )),
                BindingKind::ForPat => Some((
                    "a destructuring `for` pattern",
                    "its type cannot be derived",
                    "the loop body",
                )),
                BindingKind::ArgPat => Some((
                    "a destructuring argument pattern",
                    "its type cannot be derived",
                    "the function body",
                )),
                _ => None,
            };
            if let Some((binder, reason, site)) = unannotatable {
                if reported.insert(*id) {
                    self.err(
                        binding.ident.span(),
                        unannotatable_binding_error(&name, binder, reason, site),
                    );
                }
                continue;
            }
            match self.resolve_binding_ty(*id) {
                Some(ty) => {
                    let base = if binding.kind == BindingKind::Arg {
                        self.args[id.0].mutability
                    } else {
                        binding.mutability
                    };
                    let forced = forced_mut
                        .contains(id)
                        .then(|| syn::Token![mut](binding.ident.span()));
                    fields.push(StateField {
                        ident: binding.ident.clone(),
                        mutability: base.or(forced),
                        ty,
                    });
                }
                None => {
                    if reported.insert(*id) {
                        let msg = if binding.kind == BindingKind::ForIter {
                            // The span points at the `for` head expression.
                            "cannot determine the type of this `for` loop's iterator, \
                             which is held across yield_!; iterate over a variable with \
                             an explicit type annotation: \
                             `let items: Type = ...; for x in items { ... }`"
                                .to_string()
                        } else if binding.kind == BindingKind::Delegate {
                            // The span points at the yield_all! operand.
                            "cannot determine the type of the coroutine delegated to by \
                             yield_all!, which is stored in the state while it runs; bind \
                             it to a variable with an explicit type annotation: \
                             `let sub: Type = make_sub(..);`"
                                .to_string()
                        } else {
                            format!(
                                "cannot determine the type of `{name}`, which is held across \
                                 yield_!; write an explicit type annotation: \
                                 `let {name}: Type = ...`"
                            )
                        };
                        self.err(binding.ident.span(), msg);
                    }
                }
            }
        }
        fields
    }

    /// Two distinct bindings with the same name live at the same variant
    /// entry (shadowing where the shadowed binding — often a borrow
    /// source — is still in use) cannot both become fields.
    fn check_name_collisions(
        &mut self,
        live: &BTreeSet<BindingId>,
        reported: &mut BTreeSet<(BindingId, BindingId)>,
    ) {
        let mut by_name: BTreeMap<String, BindingId> = BTreeMap::new();
        for id in live {
            let ident = self.cfg.bindings[id.0].ident.clone();
            if let Some(prev) = by_name.insert(ident.to_string(), *id)
                && reported.insert((prev, *id))
            {
                self.err(
                    ident.span(),
                    format!(
                        "two different bindings named `{ident}` are alive at the same \
                         suspension point (the shadowed one is still in use, possibly as the \
                         source of a reconstructed borrow); rename one of them"
                    ),
                );
            }
        }
    }

    /// Rejects an opaque-statement jump when a binding declared inside
    /// the statement shares its name with a field of the target variant:
    /// the transition moves the fields by name from the jump site, so it
    /// would capture the inner (shadowing) binding instead of the stored
    /// one. Conservative: the inner binding is flagged even if it does
    /// not actually enclose the jump within the statement.
    fn check_jump_shadowing(&mut self, variants: &[Variant]) {
        let cfg = self.cfg;
        let mut reported: BTreeSet<String> = BTreeSet::new();
        for blk in &cfg.blocks {
            for &j in &blk.jumps {
                let jump = &cfg.opaque_jumps[j];
                let OpaqueJumpKind::Goto { target, store } = jump.kind else {
                    // Completions move nothing; the value is evaluated
                    // in place.
                    continue;
                };
                let i = variants
                    .binary_search_by_key(&target, |v| v.block)
                    .expect("BUG: opaque jump into an inline block");
                for ident in &jump.shadowed {
                    // The jump's own store binding is provided by a
                    // fresh `let` synthesized at the jump site, so a
                    // same-named user binding cannot be captured.
                    if store.is_some_and(|s| cfg.bindings[s.0].ident == *ident) {
                        continue;
                    }
                    let name = ident.to_string();
                    if variants[i].fields.iter().any(|f| f.ident == *ident)
                        && reported.insert(name.clone())
                    {
                        self.err(
                            ident.span(),
                            format!(
                                "`{name}` is also stored in the coroutine state by the \
                                 `break`/`continue` in this statement, which would capture \
                                 this inner `{name}` instead of the stored one; rename the \
                                 inner binding"
                            ),
                        );
                    }
                }
            }
        }
    }

    /// The type of a binding: signature type for arguments, the resume
    /// type for unannotated resume bindings, otherwise the recursively
    /// resolved syntactic type source. Move dependencies always point at
    /// earlier bindings, so the recursion terminates.
    fn resolve_binding_ty(&self, id: BindingId) -> Option<syn::Type> {
        let binding = &self.cfg.bindings[id.0];
        match binding.kind {
            BindingKind::Arg => return Some(self.args[id.0].ty.clone()),
            BindingKind::Resume if matches!(binding.ty, TySource::Unknown) => {
                return Some(self.resume_ty.clone());
            }
            _ => {}
        }
        self.resolve_ty_source(&binding.ty)
    }

    fn resolve_ty_source(&self, src: &TySource) -> Option<syn::Type> {
        match src {
            TySource::Unknown => None,
            TySource::Known(t) => Some(t.clone()),
            TySource::Moved(id) => self.resolve_binding_ty(*id),
            TySource::Range {
                inclusive,
                start,
                end,
            } => {
                let t = self
                    .resolve_ty_source(start)
                    .or_else(|| self.resolve_ty_source(end))?;
                Some(if *inclusive {
                    syn::parse_quote!(::core::ops::RangeInclusive<#t>)
                } else {
                    syn::parse_quote!(::core::ops::Range<#t>)
                })
            }
            TySource::IntoIter(inner) => {
                let t = self.resolve_ty_source(inner)?;
                Some(syn::parse_quote!(<#t as ::core::iter::IntoIterator>::IntoIter))
            }
            TySource::IterItem(iter) => {
                let t = self.resolve_binding_ty(*iter)?;
                Some(syn::parse_quote!(<#t as ::core::iter::Iterator>::Item))
            }
        }
    }
}

/// A binding whose type can't be spelled in an annotation (e.g. a match
/// arm or destructuring `for` pattern) but that must be stored across a
/// state boundary anyway: point the user at rebinding with an explicit
/// type. `binder` names what bound it, `reason` says why it can't be
/// annotated in place, and `site` is where to insert the rebind.
fn unannotatable_binding_error(name: &str, binder: &str, reason: &str, site: &str) -> String {
    format!(
        "`{name}` is bound by {binder} and must be stored across a state boundary, but \
         {reason}; rebind it at the top of {site}: `let {name}2: Type = {name};`"
    )
}

/// Computes each block's region root by walking the unique-predecessor
/// chain of inline blocks. Inline blocks always have exactly one
/// predecessor and cannot form cycles (a cycle of single-predecessor
/// blocks would be unreachable and already removed).
fn region_roots(cfg: &Cfg) -> Vec<BlockId> {
    let mut pred = vec![usize::MAX; cfg.blocks.len()];
    for (b, blk) in cfg.blocks.iter().enumerate() {
        for s in blk.terminator.successors() {
            pred[s] = b;
        }
    }
    (0..cfg.blocks.len())
        .map(|mut b| {
            while cfg.blocks[b].inline {
                b = pred[b];
            }
            b
        })
        .collect()
}

fn def_blocks(cfg: &Cfg) -> Vec<Option<BlockId>> {
    let mut out = vec![None; cfg.bindings.len()];
    for (b, blk) in cfg.blocks.iter().enumerate() {
        for id in &blk.defs {
            out[id.0] = Some(b);
        }
    }
    out
}

// === Variant naming ===

/// Assigns variant names: `Start` for the entry, `S{k}` for resume points
/// (source yield order = block creation order, which block ids preserve),
/// `B{k}` for the remaining variant blocks (reverse postorder). Inline
/// blocks get no name. Both numberings are deterministic for a given
/// source, which serde representations rely on.
fn variant_idents(cfg: &Cfg) -> BTreeMap<BlockId, syn::Ident> {
    let mut idents = BTreeMap::new();
    idents.insert(cfg.entry, format_ident!("Start"));
    let mut s = 0;
    for (b, block) in cfg.blocks.iter().enumerate() {
        if block.resume_point {
            s += 1;
            idents.insert(b, format_ident!("S{s}"));
        }
    }
    let mut k = 0;
    for b in reverse_postorder(cfg) {
        if !idents.contains_key(&b) && !cfg.blocks[b].inline {
            k += 1;
            idents.insert(b, format_ident!("B{k}"));
        }
    }
    idents
}

fn reverse_postorder(cfg: &Cfg) -> Vec<BlockId> {
    let mut visited = vec![false; cfg.blocks.len()];
    let mut post = Vec::new();
    // Iterative DFS; `true` marks a node whose successors are done.
    let mut stack = vec![(cfg.entry, false)];
    while let Some((b, expanded)) = stack.pop() {
        if expanded {
            post.push(b);
            continue;
        }
        if std::mem::replace(&mut visited[b], true) {
            continue;
        }
        stack.push((b, true));
        // Jump edges included: a block reachable only through an opaque
        // jump still becomes a variant and needs a name.
        for s in cfg.successors(b).rev() {
            if !visited[s] {
                stack.push((s, false));
            }
        }
    }
    post.reverse();
    post
}

/// The expressions a terminator evaluates in its own block (before any
/// transition), for statement-level use scans.
fn terminator_exprs(t: &Terminator) -> Vec<&syn::Expr> {
    match t {
        Terminator::Goto(_) => Vec::new(),
        Terminator::Branch { cond, .. } => vec![cond],
        Terminator::Match { scrutinee, arms } => std::iter::once(scrutinee)
            .chain(arms.iter().filter_map(|a| a.guard.as_ref()))
            .collect(),
        Terminator::Yield { value, .. } => vec![value],
        Terminator::IterNext { .. } => Vec::new(),
        Terminator::Return(e) => vec![e],
    }
}

#[cfg(test)]
mod tests;
