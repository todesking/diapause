//! CFG-based liveness analysis, type determination, and borrow
//! reconstruction. Consumes the CFG built by `lower.rs`.
//!
//! Every block that is not inlined into its predecessor's arm becomes a
//! state-enum variant; its fields are the bindings live at its entry.
//! Direct borrows never enter a variant: their uses are attributed to
//! the borrowed binding instead, and the borrow is re-established at the
//! head of every arm (region) that uses it outside its defining region.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use quote::{ToTokens, format_ident};
use syn::visit::Visit;

use crate::cfg::{
    BindingId, BindingKind, BlockId, BorrowSource, Cfg, OpaqueJumpKind, Terminator, TySource,
};
use crate::lower::{
    ErrorSink, PatBindingCollector, UseCollector, collect_markers, collect_token_idents,
};

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
    /// Whether `source` names a tracked local binding. A borrow of a
    /// non-local name (`static`/`const`, or a name rustc will reject) is
    /// still rebuilt verbatim, but its source is not a variant field or
    /// an earlier reborrow, so the IR self-check must not require it to
    /// be one.
    pub source_is_local: bool,
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

/// A resume-point variant whose dispatch arm can be generated in place
/// (see `expand`): every suspension reachable from the resume without
/// crossing another variant's dispatch re-enters this same variant, so
/// the arm can bind the fields by `ref mut` and skip both the
/// whole-enum move-out at `__drive` entry and the whole-enum write-back
/// at the yield. Paths that leave the cycle (completion, a yield into a
/// different variant) first move the state out (`expand`'s rehydrate)
/// and continue with the standard by-value codegen.
///
/// The cost of the in-place arm is the panic guarantee: a panic inside
/// it leaves the partially updated `Suspended` variant behind instead
/// of `Poisoned` (resuming such a state is memory-safe but its behavior
/// is unspecified). The macro's `in_place = false` argument disables
/// the plans entirely.
#[derive(Debug)]
pub struct InPlacePlan {
    /// The resume-point block whose arm is generated in place.
    pub block: BlockId,
    /// Blocks emitted inside the fast arm, `block` included. Closed
    /// under non-yield terminator successors, and a tree rooted at
    /// `block` (every other member has exactly one region-internal
    /// predecessor), so codegen inlines each member exactly once.
    pub region: BTreeSet<BlockId>,
    /// The region members from which an in-region yield is reachable,
    /// `block` included: these run in in-place mode (uses of stored
    /// names rewritten to dereferences). The remaining, cold members
    /// (completion paths) run the standard by-value codegen after the
    /// state is moved back out at the hot→cold edge.
    pub hot: BTreeSet<BlockId>,
    /// The variant's fields as bindings: bound by `ref mut` in the fast
    /// arm, with their uses rewritten to dereferences.
    pub fields: BTreeSet<BindingId>,
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
    // Consumed by the unit tests and `validate`; codegen uses `variants`.
    pub live_in: Vec<BTreeSet<BindingId>>,
    /// Indexed by `BlockId`: upward-exposed uses after borrow
    /// substitution — the `use` sets the liveness fixed point ran on.
    // Consumed by `validate`, which re-checks the dataflow equations.
    pub uses: Vec<BTreeSet<BindingId>>,
    /// Resume-point variants whose dispatch arm is generated in place,
    /// in ascending block order. Empty when the optimization is
    /// disabled (`in_place = false`) or no variant qualifies.
    pub in_place: Vec<InPlacePlan>,
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
    analyze_with(cfg, args, resume_ty, true)
}

/// [`analyze`] with the in-place optimization toggled by `in_place`
/// (the macro's `in_place` argument): when `false`, no [`InPlacePlan`]s
/// are computed and codegen always moves the state out and back.
pub fn analyze_with(
    cfg: &Cfg,
    args: &[ArgInfo],
    resume_ty: &syn::Type,
    in_place: bool,
) -> syn::Result<Analysis> {
    let cx = Context::new(cfg, args, resume_ty);
    cx.run(in_place)
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

    fn run(mut self, in_place: bool) -> syn::Result<Analysis> {
        let (uses, rebuilds) = self.substitute_borrows();
        let live_in = self.liveness(&uses);
        let removed_stmts = self.removed_statements(&rebuilds);
        let mut collisions: BTreeSet<(BindingId, BindingId)> = BTreeSet::new();
        let variants = self.build_variants(&live_in, &rebuilds, &mut collisions);
        self.check_jump_shadowing(&variants);
        self.check_transfer_shadowing(&live_in, &removed_stmts, &mut collisions);
        let in_place = if in_place {
            compute_in_place(self.cfg, &live_in, &uses, &variants)
        } else {
            Vec::new()
        };
        self.errors.into_result(Analysis {
            variants,
            removed_stmts,
            live_in,
            uses,
            in_place,
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
                    source,
                    mutable,
                } = &b.borrow
                else {
                    unreachable!("BUG: rebuild of a non-borrow binding")
                };
                Reborrow {
                    target: b.ident.clone(),
                    target_mut: b.mutability,
                    source: source_ident.clone(),
                    mutable: *mutable,
                    source_is_local: source.is_some(),
                }
            })
            .collect()
    }
}

// === Variant fields, type determination, and errors ===

impl Context<'_> {
    /// Builds the `Variant` list: one entry per non-inline block, in
    /// ascending `BlockId` order, combining its fields, its reborrows, and
    /// the name assigned by `variant_idents`. Name-collision pairs are
    /// accumulated into `collisions`, shared with the transfer-shadowing
    /// check so one conflicting pair is only reported once.
    fn build_variants(
        &mut self,
        live_in: &[BTreeSet<BindingId>],
        rebuilds: &[BTreeSet<BindingId>],
        collisions: &mut BTreeSet<(BindingId, BindingId)>,
    ) -> Vec<Variant> {
        let forced_mut = self.compute_forced_mut(rebuilds);
        let mut reported: BTreeSet<BindingId> = BTreeSet::new();
        let idents = variant_idents(self.cfg);
        let mut variants = Vec::new();
        for b in 0..self.cfg.blocks.len() {
            if self.cfg.blocks[b].inline {
                continue;
            }
            let fields =
                self.build_fields_for_block(&live_in[b], &forced_mut, &mut reported, collisions);
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
                                "this `{name}` shadows a variable that the \
                                 `break`/`continue` in this statement must store into the \
                                 coroutine state; the jump would capture this inner \
                                 `{name}` in place of the outer one; rename the inner \
                                 binding"
                            ),
                        );
                    }
                }
            }
        }
    }

    /// Rejects a binding that a by-name state transfer would capture in
    /// place of the (different, same-named) live binding it moves. A
    /// transfer — a terminator edge into a non-inline block, or an
    /// opaque jump — moves the target's live-in bindings by name from
    /// the transfer site, where every earlier definition of the emitted
    /// inline chain is still in scope; a live binding shadowed there
    /// (it survives its shadow as the source of a reconstructed borrow,
    /// or because its use sits past the shadow's original scope, which
    /// lowering flattens away) would be transferred with the shadowing
    /// binding's value — a silent wrong-value miscompile. Definitions
    /// that never reach the emitted arm (jump-store `let`s scoped inside
    /// their marker's braces, removed borrow `let`s) shadow nothing.
    /// Stricter than `validate::check_transition_shadowing`, which keeps
    /// backstopping codegen in debug builds.
    fn check_transfer_shadowing(
        &mut self,
        live_in: &[BTreeSet<BindingId>],
        removed_stmts: &[BTreeSet<usize>],
        reported: &mut BTreeSet<(BindingId, BindingId)>,
    ) {
        let cfg = self.cfg;
        let n = cfg.blocks.len();
        // Unique terminator predecessor, for walking inline chains
        // (inline blocks have exactly one, and it is a terminator edge).
        let mut term_pred = vec![usize::MAX; n];
        for b in 0..n {
            for s in cfg.blocks[b].terminator.successors() {
                term_pred[s] = b;
            }
        }
        let iter_heads = iter_body_heads(cfg);
        for c in 0..n {
            // The inline chain whose emitted arm ends with c's
            // transfers, root first (= textual order of the arm).
            let mut chain = vec![c];
            while cfg.blocks[*chain.last().unwrap()].inline && chain.len() <= n {
                let p = term_pred[*chain.last().unwrap()];
                if p == usize::MAX {
                    break;
                }
                chain.push(p);
            }
            chain.reverse();
            // Every definition the emitted arm places in scope, with its
            // textual position.
            let mut defs: Vec<(usize, DefPos, BindingId)> = Vec::new();
            for (ci, &b) in chain.iter().enumerate() {
                let stores = self.jump_store_bindings(b);
                for &id in &cfg.blocks[b].defs {
                    let removed = cfg.bindings[id.0]
                        .def_stmt
                        .is_some_and(|i| removed_stmts[b].contains(&i));
                    if !stores.contains(&id) && !removed {
                        defs.push((ci, self.def_pos(id, b, &iter_heads), id));
                    }
                }
            }
            for t in cfg.blocks[c].terminator.successors() {
                if !cfg.blocks[t].inline {
                    self.check_transfer_edge(t, &defs, None, None, live_in, reported);
                }
            }
            if !cfg.blocks[c].jumps.is_empty() {
                let markers = marker_stmt_indices(&cfg.blocks[c].stmts);
                let c_idx = chain.len() - 1;
                for &k in &cfg.blocks[c].jumps {
                    let OpaqueJumpKind::Goto { target, store } = cfg.opaque_jumps[k].kind else {
                        continue;
                    };
                    // A missing marker cannot happen (codegen would drop
                    // the jump); statement 0 errs toward acceptance.
                    let m = markers.get(&k).copied().unwrap_or(0);
                    self.check_transfer_edge(
                        target,
                        &defs,
                        Some((c_idx, m)),
                        store,
                        live_in,
                        reported,
                    );
                }
            }
        }
    }

    /// One transfer edge into non-inline `t`. `limit` is `Some((chain
    /// index of the jump's block, marker statement index))` for an
    /// opaque jump, which fires mid-chain: definitions at or after the
    /// marker statement are not yet in scope there. `store` is the
    /// jump's own destination binding, provided by a fresh `let`
    /// synthesized at the jump site that no user binding can shadow.
    fn check_transfer_edge(
        &mut self,
        t: BlockId,
        defs: &[(usize, DefPos, BindingId)],
        limit: Option<(usize, usize)>,
        store: Option<BindingId>,
        live_in: &[BTreeSet<BindingId>],
        reported: &mut BTreeSet<(BindingId, BindingId)>,
    ) {
        let cfg = self.cfg;
        let in_scope = |ci: usize, pos: DefPos| match limit {
            None => true,
            Some((li, m)) => {
                ci < li
                    || match pos {
                        DefPos::Head => true,
                        DefPos::Stmt(i) => i < m,
                        DefPos::Unknown => false,
                    }
            }
        };
        for &id in &live_in[t] {
            let name = &cfg.bindings[id.0].ident;
            if store.is_some_and(|s| cfg.bindings[s.0].ident == *name) {
                continue;
            }
            let own = defs
                .iter()
                .find(|&&(_, _, d)| d == id)
                .map(|&(ci, pos, _)| (ci, pos));
            for &(ci, pos, other) in defs {
                if other == id || cfg.bindings[other.0].ident != *name || !in_scope(ci, pos) {
                    continue;
                }
                // A binding the chain does not define is unpacked from a
                // state field at the arm head, so any same-named chain
                // definition shadows it; one the chain does define is
                // shadowed only by a definition provably after its own.
                let captured = own.is_none_or(|o| definitely_after((ci, pos), o));
                if captured && reported.insert((id.min(other), id.max(other))) {
                    self.err(
                        cfg.bindings[other.0].ident.span(),
                        format!(
                            "this `{name}` shadows an earlier binding `{name}` that is \
                             still in use across a suspension point (possibly as the \
                             source of a reconstructed borrow); the coroutine state \
                             would capture this shadowing `{name}` in place of the \
                             earlier one; rename one of them"
                        ),
                    );
                }
            }
        }
    }

    /// Destination bindings of the opaque jumps owned by `b`: their
    /// synthesized `let`s are scoped inside the jump markers' braces and
    /// never shadow anything outside.
    fn jump_store_bindings(&self, b: BlockId) -> BTreeSet<BindingId> {
        self.cfg.blocks[b]
            .jumps
            .iter()
            .filter_map(|&k| match self.cfg.opaque_jumps[k].kind {
                OpaqueJumpKind::Goto { store: Some(s), .. } => Some(s),
                _ => None,
            })
            .collect()
    }

    /// The textual position of `id`'s definition within `block`.
    /// `iter_heads` marks the simple `for` loop variables, which are
    /// `BindingKind::Local` without a `def_stmt` (bound by the
    /// `IterNext` pattern, not a `let`) yet sit at the block head; the
    /// remaining such locals are `push_store`-synthesized `let`s whose
    /// statement index is not tracked.
    fn def_pos(
        &self,
        id: BindingId,
        block: BlockId,
        iter_heads: &BTreeMap<BlockId, String>,
    ) -> DefPos {
        let b = &self.cfg.bindings[id.0];
        match b.def_stmt {
            Some(i) => DefPos::Stmt(i),
            None if b.kind != BindingKind::Local => DefPos::Head,
            None if iter_heads.get(&block).is_some_and(|n| b.ident == n) => DefPos::Head,
            None => DefPos::Unknown,
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
            TySource::Boxed(inner) => {
                let t = self.resolve_ty_source(inner)?;
                Some(syn::parse_quote!(::diapause::__private::Box<#t>))
            }
            TySource::IntoIter(inner) => {
                let t = self.resolve_ty_source(inner)?;
                Some(syn::parse_quote!(<#t as ::core::iter::IntoIterator>::IntoIter))
            }
            TySource::RangeInclusiveIter(inner) => {
                // Lowering only wraps heads it typed as `RangeInclusive`
                // itself, so the resolved inner type always has the
                // `RangeInclusive<T>` shape to project the endpoint
                // type from.
                let t = self.resolve_ty_source(inner)?;
                let item = range_inclusive_item(&t)?;
                Some(syn::parse_quote!(__RangeInclusiveIter<#item>))
            }
            TySource::IterItem(iter) => {
                let t = self.resolve_binding_ty(*iter)?;
                Some(syn::parse_quote!(<#t as ::core::iter::Iterator>::Item))
            }
        }
    }
}

/// Textual position of a definition within its block, for ordering
/// same-named definitions along an inline chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DefPos {
    /// Bound at the block head: resume bindings, match/`for` patterns.
    Head,
    /// A `let` at this statement index.
    Stmt(usize),
    /// A `push_store`-synthesized `let`: no statement index is tracked
    /// (and block merging can leave it anywhere in the block).
    Unknown,
}

/// Whether the definition at `a` is provably later in the emitted arm
/// than the one at `b`, each position being (index into the chain,
/// position within the block). Different blocks are ordered by chain
/// depth; within one block, `Head` precedes every statement and
/// `Unknown` cannot be ordered (erring toward acceptance — the
/// validation pass still backstops codegen).
fn definitely_after(a: (usize, DefPos), b: (usize, DefPos)) -> bool {
    if a.0 != b.0 {
        return a.0 > b.0;
    }
    match (a.1, b.1) {
        (DefPos::Stmt(i), DefPos::Stmt(j)) => i > j,
        (DefPos::Stmt(_), DefPos::Head) => true,
        _ => false,
    }
}

/// The simple loop-variable name bound at the head of each `IterNext`
/// body block (see `Context::def_pos`).
fn iter_body_heads(cfg: &Cfg) -> BTreeMap<BlockId, String> {
    let mut out = BTreeMap::new();
    for blk in &cfg.blocks {
        if let Terminator::IterNext { pat, body, .. } = &blk.terminator
            && let syn::Pat::Ident(pi) = &**pat
        {
            out.insert(*body, pi.ident.to_string());
        }
    }
    out
}

/// Statement index of each `__diapause_jump!(k, ..)` marker embedded in
/// `stmts`, keyed by `k`.
fn marker_stmt_indices(stmts: &[syn::Stmt]) -> BTreeMap<usize, usize> {
    let mut out = BTreeMap::new();
    for (i, stmt) in stmts.iter().enumerate() {
        let mut ks = Vec::new();
        collect_markers(stmt.to_token_stream(), &mut ks);
        for k in ks {
            out.entry(k).or_insert(i);
        }
    }
    out
}

/// Extracts `T` from a `RangeInclusive<T>`-shaped type.
fn range_inclusive_item(ty: &syn::Type) -> Option<&syn::Type> {
    let syn::Type::Path(p) = ty else { return None };
    let seg = p.path.segments.last()?;
    if seg.ident != "RangeInclusive" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    match args.args.first() {
        Some(syn::GenericArgument::Type(t)) if args.args.len() == 1 => Some(t),
        _ => None,
    }
}

/// A binding whose type can't be spelled in an annotation (e.g. a match
/// arm or destructuring `for` pattern) but that is held across yield_!
/// and must be stored in the coroutine state anyway: point the user at
/// rebinding with an explicit type. `binder` names what bound it,
/// `reason` says why it can't be annotated in place, and `site` is where
/// to insert the rebind.
fn unannotatable_binding_error(name: &str, binder: &str, reason: &str, site: &str) -> String {
    format!(
        "`{name}` is bound by {binder} and held across yield_!, so it must be stored \
         in the coroutine state, but {reason}; rebind it with an explicit type at the \
         top of {site}: `let {name}2: Type = {name};`"
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

// === In-place resume arms ===

/// One [`InPlacePlan`] per qualifying resume-point variant, in
/// ascending block order (inherited from `variants`).
fn compute_in_place(
    cfg: &Cfg,
    live_in: &[BTreeSet<BindingId>],
    uses: &[BTreeSet<BindingId>],
    variants: &[Variant],
) -> Vec<InPlacePlan> {
    variants
        .iter()
        .filter(|v| cfg.blocks[v.block].resume_point)
        .filter_map(|v| in_place_plan(cfg, live_in, uses, variants, v))
        .collect()
}

/// Decides whether `v`'s dispatch arm can be generated in place, and
/// over which region. Deliberately conservative: any construct the fast
/// codegen cannot handle transparently (opaque jumps, region cycles or
/// joins, shadowing of stored names, macros or early-exit rewrites that
/// mention them, likely moves of stored bindings) makes the variant
/// fall back to the standard move-out arm, which is always correct.
fn in_place_plan(
    cfg: &Cfg,
    live_in: &[BTreeSet<BindingId>],
    uses: &[BTreeSet<BindingId>],
    variants: &[Variant],
    v: &Variant,
) -> Option<InPlacePlan> {
    let s = v.block;
    // The bindings stored as fields: exactly the storable live-ins.
    // (Bindings the analysis rejected — non-reconstructible borrows,
    // unannotatable patterns — already produced an error, so the plan
    // never reaches codegen in that case.)
    let fields: BTreeSet<BindingId> = live_in[s]
        .iter()
        .copied()
        .filter(|id| matches!(cfg.bindings[id.0].borrow, BorrowSource::NotABorrow))
        .collect();
    let field_names: BTreeSet<String> = fields
        .iter()
        .map(|id| cfg.bindings[id.0].ident.to_string())
        .collect();

    // Region discovery: follow terminator edges, stopping at yields
    // (which must all re-enter `s`) and returns. Opaque jumps
    // transition via `continue '__dispatch`, which does not exist on
    // the fast path.
    let mut region = BTreeSet::from([s]);
    let mut queue = vec![s];
    let mut has_yield_back = false;
    while let Some(b) = queue.pop() {
        let blk = &cfg.blocks[b];
        if !blk.jumps.is_empty() {
            return None;
        }
        if let Terminator::Yield { next, .. } = blk.terminator {
            if next != s {
                return None;
            }
            has_yield_back = true;
            continue;
        }
        for t in blk.terminator.successors() {
            // A non-yield edge into `s` or into another resume point
            // cannot occur (resume points are entered only by their
            // yield); the entry's statements assume the `Start` unpack.
            if t == s || cfg.blocks[t].resume_point || t == cfg.entry {
                return None;
            }
            if region.insert(t) {
                queue.push(t);
            }
        }
    }
    // Without an in-region yield the fast arm would only add a state
    // copy (rehydrate) to the standard exit path.
    if !has_yield_back {
        return None;
    }

    // The region must be a tree rooted at `s`: each member reached by
    // exactly one region edge, so codegen inlines it exactly once.
    // This also excludes intra-resume cycles, which need the dispatch
    // loop, and joins, which would duplicate code.
    let mut preds: BTreeMap<BlockId, usize> = BTreeMap::new();
    for &b in &region {
        if matches!(cfg.blocks[b].terminator, Terminator::Yield { .. }) {
            continue;
        }
        for t in cfg.blocks[b].terminator.successors() {
            *preds.entry(t).or_insert(0) += 1;
        }
    }
    if region.iter().any(|&b| b != s && preds.get(&b) != Some(&1)) {
        return None;
    }

    // Hot blocks: those from which an in-region yield is reachable;
    // they run in in-place mode. The remaining, cold blocks (completion
    // paths) run the standard by-value codegen after a hot→cold edge
    // moves the state back out, so the constraints below apply to hot
    // blocks only. `s` is always hot (the region is discovered from it
    // and contains a yield).
    let mut hot: BTreeSet<BlockId> = region
        .iter()
        .copied()
        .filter(|&b| matches!(cfg.blocks[b].terminator, Terminator::Yield { .. }))
        .collect();
    fixpoint(region.iter().copied(), |b| {
        if !hot.contains(&b)
            && cfg.blocks[b]
                .terminator
                .successors()
                .any(|t| hot.contains(&t))
        {
            hot.insert(b);
            true
        } else {
            false
        }
    });
    for &b in &region {
        if hot.contains(&b) {
            continue;
        }
        // Cold code is emitted by the standard codegen, which can only
        // reach blocks that standard inlining reaches (the fast arm has
        // no `__state`/`'__dispatch` to transition through).
        if !cfg.blocks[b].inline {
            return None;
        }
        // A binding defined in a hot block and consumed cold would be
        // read after the rehydration moved the state out; its rewritten
        // definition may borrow the state (e.g. `let p = &buf[0];`), so
        // keep such variants on the standard arm.
        if uses[b]
            .iter()
            .any(|id| hot.iter().any(|&h| cfg.blocks[h].defs.contains(id)))
        {
            return None;
        }
    }

    // Definitions inside the hot region: a different binding shadowing
    // a stored name would be captured by the name-based rewrite; the
    // stored binding itself may be re-bound only at a known position (a
    // `let` statement or the head pattern of the region `for` that owns
    // it), so codegen can switch from dereferences to the fresh local
    // there and write the local back into the field at the yield.
    let head_pats = iter_head_pat_names(cfg, &region);
    for &b in &hot {
        for &id in &cfg.blocks[b].defs {
            let name = cfg.bindings[id.0].ident.to_string();
            if name.starts_with("__self_") {
                return None;
            }
            if fields.contains(&id) {
                let at_head = head_pats.get(&b).is_some_and(|p| p.contains(&name));
                if cfg.bindings[id.0].def_stmt.is_none() && !at_head {
                    return None;
                }
            } else if field_names.contains(&name) {
                return None;
            }
        }
        // Reborrows are rebuilt as locals at the head of a variant
        // block's code; one named like a field would shadow the
        // rewrite's dereference targets.
        if let Ok(i) = variants.binary_search_by_key(&b, |v| v.block)
            && variants[i]
                .reborrows
                .iter()
                .any(|rb| field_names.contains(&rb.target.to_string()))
        {
            return None;
        }
    }

    // Statement- and terminator-level scans over the hot blocks for
    // constructs the rewrite cannot see through. (Cold statements and
    // terminators run standard, un-rewritten code.)
    for &b in &hot {
        for stmt in &cfg.blocks[b].stmts {
            if !stmt_is_in_place_safe(stmt, &field_names) {
                return None;
            }
        }
        let safe = match &cfg.blocks[b].terminator {
            Terminator::Branch { cond, .. } => expr_is_in_place_safe(cond, &field_names),
            Terminator::Match { scrutinee, arms } => {
                // A match on a bare stored binding usually moves it
                // (standard codegen matches on the unpacked local); the
                // rewritten `match (*__self_x)` would not compile for
                // non-`Copy` payloads, so keep such variants on the
                // standard arm. An arm binding values into a cold body
                // would carry data derived from the rewritten scrutinee
                // (possibly borrowing the state) past the rehydration.
                !is_bare_field(scrutinee, &field_names)
                    && expr_is_in_place_safe(scrutinee, &field_names)
                    && arms.iter().all(|a| {
                        (hot.contains(&a.body) || !pat_binds_anything(&a.pat))
                            && a.guard
                                .as_ref()
                                .is_none_or(|g| expr_is_in_place_safe(g, &field_names))
                    })
            }
            Terminator::Yield { value, .. } => expr_is_in_place_safe(value, &field_names),
            // A hot block never ends in `Return`/`Unreachable` (no
            // yield would be reachable from it).
            Terminator::Goto(_)
            | Terminator::IterNext { .. }
            | Terminator::Return(_)
            | Terminator::Unreachable(_) => true,
        };
        if !safe {
            return None;
        }
    }

    Some(InPlacePlan {
        block: s,
        region,
        hot,
        fields,
    })
}

/// Whether a pattern binds at least one identifier.
fn pat_binds_anything(pat: &syn::Pat) -> bool {
    let mut c = PatBindingCollector::default();
    c.visit_pat(pat);
    !c.bindings.is_empty()
}

/// The names bound by the head pattern of each region-internal
/// `IterNext` body: the only place a stored binding may be re-bound
/// without a statement index.
fn iter_head_pat_names(
    cfg: &Cfg,
    region: &BTreeSet<BlockId>,
) -> BTreeMap<BlockId, BTreeSet<String>> {
    let mut out = BTreeMap::new();
    for &b in region {
        if let Terminator::IterNext { pat, body, .. } = &cfg.blocks[b].terminator {
            let mut c = PatBindingCollector::default();
            c.visit_pat(pat);
            out.insert(
                *body,
                c.bindings.into_iter().map(|(i, _)| i.to_string()).collect(),
            );
        }
    }
    out
}

/// Whether `e` is a bare single-segment path naming a stored binding —
/// the shape whose evaluation in value position is (potentially) a
/// move.
fn is_bare_field(e: &syn::Expr, field_names: &BTreeSet<String>) -> bool {
    matches!(e, syn::Expr::Path(p)
        if p.qself.is_none()
            && p.path.leading_colon.is_none()
            && p.path.segments.len() == 1
            && p.path.segments[0].arguments.is_none()
            && field_names.contains(&p.path.segments[0].ident.to_string()))
}

/// Statement-level in-place safety. A `let`'s own top-level pattern is
/// exempt from the binding scan (its bindings are block defs, checked
/// against the plan separately); everything else — including patterns
/// nested inside expressions — goes through [`InPlaceScan`].
fn stmt_is_in_place_safe(stmt: &syn::Stmt, field_names: &BTreeSet<String>) -> bool {
    match stmt {
        syn::Stmt::Local(l) => {
            let Some(init) = &l.init else { return true };
            // `let y = x;` of a stored `x` usually moves it; the
            // rewritten `let y = (*__self_x);` would not compile for
            // non-`Copy` types.
            if is_bare_field(&init.expr, field_names) {
                return false;
            }
            expr_is_in_place_safe(&init.expr, field_names)
                && init
                    .diverge
                    .as_ref()
                    .is_none_or(|(_, e)| expr_is_in_place_safe(e, field_names))
        }
        // Nested items are separate scopes: not rewritten, and unable
        // to capture locals, so whatever they contain is safe.
        syn::Stmt::Item(_) => true,
        syn::Stmt::Expr(e, _) => expr_is_in_place_safe(e, field_names),
        syn::Stmt::Macro(m) => macro_is_in_place_safe(&m.mac, field_names),
    }
}

fn expr_is_in_place_safe(e: &syn::Expr, field_names: &BTreeSet<String>) -> bool {
    let mut scan = InPlaceScan {
        field_names,
        ok: true,
    };
    scan.visit_expr(e);
    scan.ok
}

/// A macro's tokens are spliced verbatim, invisible to the rewrite: any
/// mention of a stored name (or of the generated `self`/`__self_*`
/// names) inside disqualifies the variant.
fn macro_is_in_place_safe(mac: &syn::Macro, field_names: &BTreeSet<String>) -> bool {
    let mut idents = HashSet::new();
    collect_token_idents(mac.tokens.clone(), &mut idents);
    !idents
        .iter()
        .any(|i| field_names.contains(i) || i == "self" || i.starts_with("__self_"))
}

/// Syntactic conditions the in-place rewrite cannot handle, collected
/// in one pass over an expression:
///
/// - a pattern binding a stored name (or `__self_*`): the binding would
///   shadow the name mid-statement, which the flat rewrite cannot see;
/// - any path mentioning `self` or `__self_*`: `self` appears only in
///   the early-exit rewrites (`return`/`?`), which assign `*self` and
///   would conflict with the arm's field borrows;
/// - a macro invocation mentioning a stored name (tokens are spliced
///   verbatim);
/// - `match`/`for`/`if let`/`while let` consuming a bare stored
///   binding: usually a move, which dereferencing breaks for non-`Copy`
///   types.
///
/// Nested items are skipped: separate scopes that cannot capture
/// locals. Closures are scanned (they capture and are rewritten).
struct InPlaceScan<'a> {
    field_names: &'a BTreeSet<String>,
    ok: bool,
}

impl<'ast> Visit<'ast> for InPlaceScan<'_> {
    fn visit_pat_ident(&mut self, pi: &'ast syn::PatIdent) {
        let name = pi.ident.to_string();
        if self.field_names.contains(&name) || name.starts_with("__self_") {
            self.ok = false;
        }
        syn::visit::visit_pat_ident(self, pi);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        for seg in &path.segments {
            if seg.ident == "self" || seg.ident.to_string().starts_with("__self_") {
                self.ok = false;
            }
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if !macro_is_in_place_safe(mac, self.field_names) {
            self.ok = false;
        }
        syn::visit::visit_macro(self, mac);
    }

    fn visit_expr(&mut self, e: &'ast syn::Expr) {
        let scrutinee = match e {
            syn::Expr::Match(m) => Some(&*m.expr),
            syn::Expr::ForLoop(f) => Some(&*f.expr),
            syn::Expr::Let(l) => Some(&*l.expr),
            _ => None,
        };
        if scrutinee.is_some_and(|s| is_bare_field(s, self.field_names)) {
            self.ok = false;
        }
        syn::visit::visit_expr(self, e);
    }

    fn visit_local(&mut self, l: &'ast syn::Local) {
        if let Some(init) = &l.init
            && is_bare_field(&init.expr, self.field_names)
        {
            self.ok = false;
        }
        syn::visit::visit_local(self, l);
    }

    fn visit_item(&mut self, _: &'ast syn::Item) {}
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
        Terminator::Return(e) | Terminator::Unreachable(e) => vec![e],
    }
}

#[cfg(test)]
mod tests;
