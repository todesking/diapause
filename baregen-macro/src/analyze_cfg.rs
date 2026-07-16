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

use crate::cfg::{BindingId, BindingKind, BlockId, BorrowSource, Cfg, Terminator, TySource};
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
        let mut rebuilds: Vec<BTreeSet<BindingId>> =
            vec![BTreeSet::new(); self.cfg.blocks.len()];
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
                    unreachable!()
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
        matches!(
            self.cfg.bindings[id.0].borrow,
            BorrowSource::Direct { .. }
        ) && matches!(self.def_block[id.0], Some(d) if self.region[d] != root)
    }

    /// Standard backward dataflow to a fixed point (the CFG has back
    /// edges): `live_in(B) = use(B) ∪ (∪ live_in(succ) ∖ def(B))`.
    fn liveness(&self, uses: &[BTreeSet<BindingId>]) -> Vec<BTreeSet<BindingId>> {
        let n = self.cfg.blocks.len();
        let mut live_in: Vec<BTreeSet<BindingId>> = vec![BTreeSet::new(); n];
        fixpoint((0..n).rev(), |b| {
            let mut set = BTreeSet::new();
            for s in self.cfg.blocks[b].terminator.successors() {
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
        for s in cfg.blocks[b].terminator.successors().rev() {
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
mod tests {
    use super::*;
    use crate::test_util::lower_args;
    use syn::parse_quote;

    fn unit() -> syn::Type {
        parse_quote!(())
    }

    fn lower_analyze(
        args: &[(&str, &str)],
        block: &syn::Block,
        resume_ty: &syn::Type,
    ) -> (Cfg, syn::Result<Analysis>) {
        let names: Vec<&str> = args.iter().map(|(n, _)| *n).collect();
        let cfg = lower_args(&names, block);
        let infos: Vec<ArgInfo> = args
            .iter()
            .map(|(_, t)| ArgInfo {
                mutability: None,
                ty: syn::parse_str(t).unwrap(),
            })
            .collect();
        let result = analyze(&cfg, &infos, resume_ty);
        (cfg, result)
    }

    fn run_args(
        args: &[(&str, &str)],
        block: &syn::Block,
        resume_ty: &syn::Type,
    ) -> (Cfg, Analysis) {
        let (cfg, result) = lower_analyze(args, block, resume_ty);
        (cfg, result.unwrap())
    }

    fn run(block: &syn::Block) -> (Cfg, Analysis) {
        run_args(&[], block, &unit())
    }

    fn error_of(block: &syn::Block) -> syn::Error {
        lower_analyze(&[], block, &unit()).1.unwrap_err()
    }

    /// Resume-point blocks, in yield order.
    fn resume_ids(cfg: &Cfg) -> Vec<BlockId> {
        (0..cfg.blocks.len())
            .filter(|b| cfg.blocks[*b].resume_point)
            .collect()
    }

    /// Field names of the variant for `block`, in field order.
    fn field_names(a: &Analysis, block: BlockId) -> Vec<String> {
        a.variant(block)
            .expect("expected a variant block")
            .fields
            .iter()
            .map(|f| f.ident.to_string())
            .collect()
    }

    fn field<'a>(a: &'a Analysis, block: BlockId, name: &str) -> &'a StateField {
        a.variant(block)
            .unwrap()
            .fields
            .iter()
            .find(|f| f.ident == name)
            .unwrap_or_else(|| panic!("no field `{name}` in block {block}"))
    }

    /// Fields of the k-th resume variant.
    fn resume_fields(cfg: &Cfg, a: &Analysis, k: usize) -> Vec<String> {
        field_names(a, resume_ids(cfg)[k])
    }

    /// Reborrows of the variant for `block`.
    fn reborrows(a: &Analysis, block: BlockId) -> &[Reborrow] {
        &a.variant(block).expect("expected a variant block").reborrows
    }

    // === Liveness and borrow reconstruction ===

    #[test]
    fn unused_vars_are_not_stored() {
        let block: syn::Block = parse_quote!({
            let a: i32 = 1;
            let b: i32 = 2;
            yield_!(1);
            a
        });
        let (cfg, a) = run(&block);
        assert_eq!(resume_ids(&cfg).len(), 1);
        assert_eq!(resume_fields(&cfg, &a, 0), ["a"]);
    }

    #[test]
    fn args_live_across_yields() {
        let block: syn::Block = parse_quote!({
            yield_!(1);
            yield_!(2);
            x
        });
        let (cfg, a) = run_args(&[("x", "u32")], &block, &unit());
        assert_eq!(resume_fields(&cfg, &a, 0), ["x"]);
        assert_eq!(resume_fields(&cfg, &a, 1), ["x"]);
        let expected: syn::Type = parse_quote!(u32);
        assert_eq!(field(&a, resume_ids(&cfg)[0], "x").ty, expected);
    }

    #[test]
    fn yield_value_use_does_not_keep_var_alive() {
        // `a` is consumed by the first yield's value expression, which is
        // evaluated before the transition, so S1 must not store it.
        let block: syn::Block = parse_quote!({
            let a: i32 = 1;
            yield_!(a);
        });
        let (cfg, a) = run(&block);
        assert!(resume_fields(&cfg, &a, 0).is_empty());
    }

    #[test]
    fn resume_binding_defaults_to_resume_type() {
        let block: syn::Block = parse_quote!({
            let r = yield_!(1);
            yield_!(2);
            r
        });
        let resume_ty: syn::Type = parse_quote!(String);
        let (cfg, a) = run_args(&[], &block, &resume_ty);
        assert!(resume_fields(&cfg, &a, 0).is_empty());
        assert_eq!(resume_fields(&cfg, &a, 1), ["r"]);
        assert_eq!(field(&a, resume_ids(&cfg)[1], "r").ty, resume_ty);
    }

    #[test]
    fn shadowing_last_def_wins() {
        let block: syn::Block = parse_quote!({
            let x: i32 = 1;
            let x: String = format!("{x}");
            yield_!(1);
            x
        });
        let (cfg, a) = run(&block);
        assert_eq!(resume_fields(&cfg, &a, 0), ["x"]);
        let expected: syn::Type = parse_quote!(String);
        assert_eq!(field(&a, resume_ids(&cfg)[0], "x").ty, expected);
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
        let (cfg, a) = run(&block);
        assert_eq!(resume_fields(&cfg, &a, 0), ["a", "b", "c", "d"]);
        let tys: Vec<syn::Type> = vec![
            parse_quote!(u8),
            parse_quote!(f32),
            parse_quote!(bool),
            parse_quote!(char),
        ];
        let fields = &a.variant(resume_ids(&cfg)[0]).unwrap().fields;
        for (f, ty) in fields.iter().zip(&tys) {
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
        let (cfg, a) = run(&block);
        assert_eq!(resume_fields(&cfg, &a, 0), ["c"]);
        let expected: syn::Type = parse_quote!(String);
        assert_eq!(field(&a, resume_ids(&cfg)[0], "c").ty, expected);
    }

    #[test]
    fn move_propagates_from_argument() {
        let block: syn::Block = parse_quote!({
            let y = x;
            yield_!(1);
            y
        });
        let (cfg, a) = run_args(&[("x", "u32")], &block, &unit());
        let expected: syn::Type = parse_quote!(u32);
        assert_eq!(field(&a, resume_ids(&cfg)[0], "y").ty, expected);
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
        let (cfg, a) = run_args(&[], &block, &resume_ty);
        assert_eq!(resume_fields(&cfg, &a, 1), ["s"]);
        assert_eq!(field(&a, resume_ids(&cfg)[1], "s").ty, resume_ty);
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
        let (cfg, a) = run(&block);
        assert_eq!(resume_fields(&cfg, &a, 0), ["a"]);
    }

    #[test]
    fn borrow_target_is_replaced_by_its_source() {
        let block: syn::Block = parse_quote!({
            let mut x: i32 = 1;
            let y = &mut x;
            yield_!(1);
            *y += 1;
        });
        let (cfg, a) = run(&block);
        let s1 = resume_ids(&cfg)[0];
        // y is reconstructed, x is stored (bound mutably for the reborrow)
        assert_eq!(field_names(&a, s1), ["x"]);
        assert!(field(&a, s1, "x").mutability.is_some());
        assert_eq!(reborrows(&a, s1).len(), 1);
        assert_eq!(reborrows(&a, s1)[0].target, "y");
        assert_eq!(reborrows(&a, s1)[0].source, "x");
        assert!(reborrows(&a, s1)[0].mutable);
        // the original `let y = &mut x;` (stmt 1 of the entry) is dropped
        assert!(a.removed_stmts[cfg.entry].contains(&1));
    }

    #[test]
    fn def_stmt_survives_goto_chain_merging() {
        // The resume block after the first yield absorbs the match's
        // join block during simplification, shifting `let y = &mut x;`
        // behind `bump();`. The removed statement must be the borrow,
        // not whatever sits at its pre-merge index.
        let block: syn::Block = parse_quote!({
            let mut x: i32 = 1;
            match () {
                _ => {
                    yield_!(1);
                    bump();
                }
            }
            let y = &mut x;
            yield_!(2);
            *y += 1;
        });
        let (cfg, a) = run(&block);
        let s1 = resume_ids(&cfg)[0];
        assert_eq!(
            cfg.blocks[s1].stmts.len(),
            2,
            "expected the join block to be merged into the resume block"
        );
        assert_eq!(a.removed_stmts[s1], BTreeSet::from([1]));
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
        let (cfg, a) = run(&block);
        assert!(a.removed_stmts[cfg.entry].is_empty());
        assert_eq!(reborrows(&a, resume_ids(&cfg)[0]).len(), 1);
    }

    #[test]
    fn shared_borrow_does_not_force_mut() {
        let block: syn::Block = parse_quote!({
            let x: String = mk();
            let y = &x;
            yield_!(1);
            y.len()
        });
        let (cfg, a) = run(&block);
        let s1 = resume_ids(&cfg)[0];
        assert_eq!(field_names(&a, s1), ["x"]);
        assert!(field(&a, s1, "x").mutability.is_none());
        assert!(!reborrows(&a, s1)[0].mutable);
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
        let (cfg, a) = run(&block);
        let s1 = resume_ids(&cfg)[0];
        assert_eq!(field_names(&a, s1), ["x"]);
        let order: Vec<_> = reborrows(&a, s1)
            .iter()
            .map(|r| r.target.to_string())
            .collect();
        assert_eq!(order, ["y", "z"]);
        // `let z = &y;` is dropped; `let y = &x;` stays because z's
        // original statement scan still sees a use of y.
        assert_eq!(a.removed_stmts[cfg.entry], BTreeSet::from([2]));
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
        let (cfg, a) = run(&block);
        assert!(resume_fields(&cfg, &a, 0).is_empty());
        assert!(a.variants.iter().all(|v| v.reborrows.is_empty()));
        assert!(a.removed_stmts.iter().all(BTreeSet::is_empty));
    }

    // === Loops and branches ===

    #[test]
    fn loop_counter_is_live_at_the_header() {
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
        let (cfg, a) = run_args(&[("n", "u32")], &block, &unit());
        let Terminator::Goto(header) = cfg.blocks[cfg.entry].terminator else {
            panic!("entry should fall into the header");
        };
        // Definition order: the argument first, then the locals.
        assert_eq!(field_names(&a, header), ["n", "sum", "i"]);
        assert!(field(&a, header, "sum").mutability.is_some());
        assert_eq!(resume_fields(&cfg, &a, 0), ["n", "sum", "i"]);
        // The entry variant holds the argument.
        assert_eq!(field_names(&a, cfg.entry), ["n"]);
    }

    #[test]
    fn branch_local_is_not_live_at_the_join() {
        let block: syn::Block = parse_quote!({
            if c {
                let a: u32 = 1;
                yield_!(1);
                f(a);
            }
            g();
        });
        let (cfg, a) = run_args(&[("c", "bool")], &block, &unit());
        let s1 = resume_ids(&cfg)[0];
        assert_eq!(field_names(&a, s1), ["a"]);
        let Terminator::Goto(join) = cfg.blocks[s1].terminator else {
            panic!("resume should goto the join");
        };
        assert!(a.live_in[join].is_empty());
        assert!(field_names(&a, join).is_empty());
    }

    #[test]
    fn branches_produce_different_field_sets() {
        let block: syn::Block = parse_quote!({
            if c {
                let a: u32 = 1;
                yield_!(1);
                f(a);
            } else {
                let b2: i64 = 2;
                yield_!(2);
                g(b2);
            }
            done();
        });
        let (cfg, a) = run_args(&[("c", "bool")], &block, &unit());
        assert_eq!(resume_fields(&cfg, &a, 0), ["a"]);
        assert_eq!(resume_fields(&cfg, &a, 1), ["b2"]);
    }

    #[test]
    fn loop_borrow_is_rebuilt_each_iteration() {
        let block: syn::Block = parse_quote!({
            let mut x: u32 = 0;
            loop {
                let y = &mut x;
                yield_!(1);
                *y += 1;
            }
        });
        let (cfg, a) = run(&block);
        let Terminator::Goto(header) = cfg.blocks[cfg.entry].terminator else {
            panic!("entry should fall into the header");
        };
        let s1 = resume_ids(&cfg)[0];
        // The borrow is defined in the header's arm and used after the
        // yield: reconstructed at the resume arm, every iteration.
        assert_eq!(reborrows(&a, s1).len(), 1);
        assert_eq!(reborrows(&a, s1)[0].target, "y");
        assert_eq!(a.removed_stmts[header], BTreeSet::from([0]));
        assert_eq!(field_names(&a, header), ["x"]);
        assert_eq!(field_names(&a, s1), ["x"]);
        assert!(field(&a, s1, "x").mutability.is_some());
    }

    #[test]
    fn borrow_from_before_the_loop_is_rebuilt_inside() {
        let block: syn::Block = parse_quote!({
            let mut x: u32 = 0;
            let y = &mut x;
            loop {
                yield_!(1);
                *y += 1;
            }
        });
        let (cfg, a) = run(&block);
        let s1 = resume_ids(&cfg)[0];
        assert_eq!(reborrows(&a, s1).len(), 1);
        assert_eq!(reborrows(&a, s1)[0].target, "y");
        assert_eq!(a.removed_stmts[cfg.entry], BTreeSet::from([1]));
        assert_eq!(field_names(&a, s1), ["x"]);
    }

    #[test]
    fn borrow_crossing_a_join_without_yield_is_rebuilt() {
        let block: syn::Block = parse_quote!({
            let x: u32 = 1;
            let y = &x;
            if c {
                yield_!(1);
            }
            f(y);
        });
        let (cfg, a) = run_args(&[("c", "bool")], &block, &unit());
        let s1 = resume_ids(&cfg)[0];
        let Terminator::Goto(join) = cfg.blocks[s1].terminator else {
            panic!("resume should goto the join");
        };
        assert_eq!(field_names(&a, s1), ["x"]);
        assert_eq!(field_names(&a, join), ["x"]);
        assert_eq!(reborrows(&a, join).len(), 1);
        assert_eq!(reborrows(&a, join)[0].target, "y");
        assert!(reborrows(&a, s1).is_empty());
        assert_eq!(a.removed_stmts[cfg.entry], BTreeSet::from([1]));
    }

    // === Range type inference ===

    #[test]
    fn range_types_are_inferred_from_either_endpoint() {
        let block: syn::Block = parse_quote!({
            let r = 0u32..k;
            let ri = a..=b;
            yield_!(1);
            f(r, ri);
        });
        let (cfg, a) = run_args(&[("a", "u64")], &block, &unit());
        let s1 = resume_ids(&cfg)[0];
        let range: syn::Type = parse_quote!(::core::ops::Range<u32>);
        let range_inclusive: syn::Type = parse_quote!(::core::ops::RangeInclusive<u64>);
        assert_eq!(field(&a, s1, "r").ty, range);
        assert_eq!(field(&a, s1, "ri").ty, range_inclusive);
    }

    #[test]
    fn range_with_unknown_endpoints_is_an_error() {
        let block: syn::Block = parse_quote!({
            let r = lo()..hi();
            yield_!(1);
            f(r);
        });
        assert!(error_of(&block).to_string().contains("type annotation"));
    }

    // === for loops ===

    #[test]
    fn for_iterator_and_loop_var_get_projected_types() {
        let block: syn::Block = parse_quote!({
            let mut sum: u32 = 0;
            for i in 0u32..n {
                yield_!(sum);
                sum += i;
            }
            sum
        });
        let (cfg, a) = run_args(&[("n", "u32")], &block, &unit());
        let s1 = resume_ids(&cfg)[0];
        // n is consumed by the range in the entry block.
        assert_eq!(field_names(&a, s1), ["sum", "__iter0", "i"]);
        let iter_ty: syn::Type = parse_quote!(
            <::core::ops::Range<u32> as ::core::iter::IntoIterator>::IntoIter
        );
        let item_ty: syn::Type = parse_quote!(
            <<::core::ops::Range<u32> as ::core::iter::IntoIterator>::IntoIter
                as ::core::iter::Iterator>::Item
        );
        assert_eq!(field(&a, s1, "__iter0").ty, iter_ty);
        assert!(field(&a, s1, "__iter0").mutability.is_some());
        assert_eq!(field(&a, s1, "i").ty, item_ty);
    }

    #[test]
    fn for_head_type_comes_from_an_annotated_variable() {
        let block: syn::Block = parse_quote!({
            let items: [u32; 3] = [1, 2, 3];
            for x in items {
                yield_!(x);
            }
        });
        let (cfg, a) = run(&block);
        let s1 = resume_ids(&cfg)[0];
        // x is consumed by the yield value; only the iterator crosses.
        assert_eq!(field_names(&a, s1), ["__iter0"]);
        let iter_ty: syn::Type =
            parse_quote!(<[u32; 3] as ::core::iter::IntoIterator>::IntoIter);
        assert_eq!(field(&a, s1, "__iter0").ty, iter_ty);
    }

    #[test]
    fn for_head_of_unknown_type_is_a_dedicated_error() {
        let block: syn::Block = parse_quote!({
            for x in items() {
                yield_!(1);
            }
        });
        let msg = error_of(&block).to_string();
        assert!(msg.contains("`for` loop's iterator"), "got: {msg}");
        assert!(!msg.contains("__iter"), "no synthetic names: {msg}");
    }

    #[test]
    fn for_destructuring_binding_crossing_a_yield_is_an_error() {
        let block: syn::Block = parse_quote!({
            let pairs: [(u32, u32); 2] = [(1, 2), (3, 4)];
            for (a, b) in pairs {
                yield_!(a);
                f(b);
            }
        });
        let msg = error_of(&block).to_string();
        assert!(msg.contains("destructuring `for` pattern"), "got: {msg}");
        assert!(msg.contains("let b2: Type = b;"), "got: {msg}");
    }

    #[test]
    fn for_destructuring_consumed_before_the_yield_is_fine() {
        let block: syn::Block = parse_quote!({
            let pairs: [(u32, u32); 2] = [(1, 2), (3, 4)];
            for (a, b) in pairs {
                yield_!(a + b);
            }
        });
        let (_, result) = lower_analyze(&[], &block, &unit());
        assert!(result.is_ok());
    }

    // === Value-position let initializers ===

    #[test]
    fn let_if_value_binding_is_carried_into_the_join() {
        let block: syn::Block = parse_quote!({
            let x: u32 = if c {
                yield_!(1);
                1
            } else {
                2
            };
            yield_!(x);
            f(x);
        });
        let (cfg, a) = run_args(&[("c", "bool")], &block, &unit());
        // The join holds x as its only field, with the annotated type.
        let s1 = resume_ids(&cfg)[0];
        let Terminator::Goto(join) = cfg.blocks[s1].terminator else {
            panic!("resume should goto the join");
        };
        assert_eq!(field_names(&a, join), ["x"]);
        let expected: syn::Type = parse_quote!(u32);
        assert_eq!(field(&a, join, "x").ty, expected);
        // x stays live across the second yield.
        assert_eq!(resume_fields(&cfg, &a, 1), ["x"]);
    }

    #[test]
    fn let_if_value_consumed_at_the_join_is_not_stored_further() {
        let block: syn::Block = parse_quote!({
            let x: u32 = if c {
                yield_!(1);
                1
            } else {
                2
            };
            yield_!(x);
        });
        let (cfg, a) = run_args(&[("c", "bool")], &block, &unit());
        // The join consumes x as the yield value; the following resume
        // state stores nothing.
        assert!(resume_fields(&cfg, &a, 1).is_empty());
    }

    #[test]
    fn let_if_value_without_annotation_is_an_error() {
        let block: syn::Block = parse_quote!({
            let x = if c {
                yield_!(1);
                1
            } else {
                2
            };
            f(x);
        });
        let (_, result) = lower_analyze(&[("c", "bool")], &block, &unit());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("type annotation"), "got: {msg}");
        assert!(msg.contains("`x`"), "got: {msg}");
    }

    #[test]
    fn let_loop_break_value_liveness() {
        let block: syn::Block = parse_quote!({
            let mut acc: u32 = 0;
            let total: u32 = loop {
                let r = yield_!(acc);
                acc += r;
                if acc > 9 {
                    yield_!(1);
                    break acc;
                }
            };
            yield_!(total);
            f(total);
        });
        let (cfg, a) = run(&block);
        // After the loop only `total` survives; `acc` dies with the break.
        let n = resume_ids(&cfg).len();
        assert_eq!(resume_fields(&cfg, &a, n - 1), ["total"]);
    }

    // === New error cases ===

    #[test]
    fn match_arm_binding_crossing_a_yield_is_an_error() {
        let block: syn::Block = parse_quote!({
            match v {
                0 => {}
                n2 => {
                    yield_!(1);
                    g(n2);
                }
            }
            done();
        });
        let (_, result) = lower_analyze(&[("v", "u32")], &block, &unit());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("match arm pattern"), "got: {msg}");
        assert!(msg.contains("let n22: Type = n2;"), "got: {msg}");
    }

    #[test]
    fn while_let_binding_crossing_a_yield_is_an_error() {
        let block: syn::Block = parse_quote!({
            while let Some(x2) = it.next() {
                yield_!(1);
                f(x2);
            }
        });
        let (_, result) = lower_analyze(&[("it", "I")], &block, &unit());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("match arm pattern"), "got: {msg}");
    }

    #[test]
    fn arm_binding_consumed_before_the_yield_is_fine() {
        let block: syn::Block = parse_quote!({
            match v {
                n2 => {
                    yield_!(n2);
                }
            }
            done();
        });
        let (_, result) = lower_analyze(&[("v", "u32")], &block, &unit());
        assert!(result.is_ok());
    }

    #[test]
    fn same_named_bindings_in_one_variant_are_an_error() {
        // The shadowed outer `x` survives as the borrow source while the
        // inner `x` is live too: both would need a field named `x`.
        let block: syn::Block = parse_quote!({
            let x: u32 = 1;
            let y = &x;
            let x: u32 = 2;
            yield_!(1);
            f(y, x);
        });
        let msg = error_of(&block).to_string();
        assert!(msg.contains("two different bindings named `x`"), "got: {msg}");
    }

    // === yield_all! delegation ===

    #[test]
    fn yield_all_stores_the_coroutine_and_the_resume_value() {
        let block: syn::Block = parse_quote!({
            let g: G = mk();
            let x: u32 = yield_all!(g);
            f(x);
        });
        let resume_ty: syn::Type = parse_quote!(u32);
        let (cfg, a) = run_args(&[], &block, &resume_ty);
        // Both suspension points store only the delegated coroutine,
        // with the type propagated from the operand variable.
        assert_eq!(resume_fields(&cfg, &a, 0), ["__dg0"]);
        assert_eq!(resume_fields(&cfg, &a, 1), ["__dg0"]);
        let g_ty: syn::Type = parse_quote!(G);
        assert_eq!(field(&a, resume_ids(&cfg)[0], "__dg0").ty, g_ty);
        // The delegation loop's header additionally carries the resume
        // value, typed by the coroutine's resume type.
        let header = a
            .variants
            .iter()
            .find(|v| v.fields.iter().any(|f| f.ident == "__rv0"))
            .expect("the loop header should hold __rv0");
        let names: Vec<String> = header.fields.iter().map(|f| f.ident.to_string()).collect();
        assert_eq!(names, ["__dg0", "__rv0"]);
        assert_eq!(field(&a, header.block, "__rv0").ty, resume_ty);
    }

    #[test]
    fn yield_all_of_an_unknown_typed_variable_is_a_dedicated_error() {
        let block: syn::Block = parse_quote!({
            let g = mk();
            yield_all!(g);
        });
        let msg = error_of(&block).to_string();
        assert!(msg.contains("delegated to by yield_all!"), "got: {msg}");
        assert!(msg.contains("type annotation"), "got: {msg}");
        assert!(!msg.contains("__dg"), "no synthetic names: {msg}");
    }

    // === Argument patterns ===

    /// Lowers and analyzes a body whose single argument `__arg0: (u32, u32)`
    /// is destructured by `pat` at the entry block.
    fn analyze_pair_arg(pat: syn::Pat, block: &syn::Block) -> (Cfg, syn::Result<Analysis>) {
        let source = syn::Ident::new("__arg0", proc_macro2::Span::call_site());
        let cfg = crate::lower::lower(&[source.clone()], &[(pat, source)], block).unwrap();
        let infos = [ArgInfo {
            mutability: None,
            ty: parse_quote!((u32, u32)),
        }];
        let result = analyze(&cfg, &infos, &unit());
        (cfg, result)
    }

    #[test]
    fn arg_pattern_bindings_not_crossing_yield_are_accepted() {
        let block: syn::Block = parse_quote!({
            let sum: u32 = a + b;
            yield_!(sum);
            sum
        });
        let (cfg, result) = analyze_pair_arg(parse_quote!((a, b)), &block);
        let a = result.unwrap();
        assert_eq!(resume_fields(&cfg, &a, 0), ["sum"]);
    }

    #[test]
    fn arg_pattern_binding_across_yield_is_rejected() {
        let block: syn::Block = parse_quote!({
            yield_!(a);
            b
        });
        let (_, result) = analyze_pair_arg(parse_quote!((a, b)), &block);
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("destructuring argument pattern"), "got: {msg}");
        assert!(msg.contains("let b2: Type = b;"), "got: {msg}");
    }
}
