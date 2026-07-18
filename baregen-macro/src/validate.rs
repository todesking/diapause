//! Self-checking validation pass over the lowered CFG and its analysis,
//! in the spirit of rustc's MIR validation.
//!
//! `expand::expand` runs it (in debug builds of the macro only, see the
//! `cfg!(debug_assertions)` gate there) right after `lower` and
//! `analyze_cfg` succeed, re-deriving the invariants the codegen relies
//! on and reporting every violation with the state and binding involved.
//! A failure indicates a miscompilation-in-the-making and is a bug in
//! baregen-macro, never a user error.
//!
//! Checked invariants:
//!
//! - **CFG well-formedness**: every edge (terminator successors and
//!   opaque-jump targets) points at an existing block, every block is
//!   reachable from the entry, inline blocks have exactly one
//!   predecessor and are neither the entry, nor resume points, nor
//!   opaque-jump targets, and every `Yield` continues into a
//!   resume-point block whose only predecessor is that yield.
//! - **Dispatch totality**: every non-inline (reachable) block has a
//!   state variant with a unique name — i.e. a dispatch arm — and the
//!   entry's variant is `Start`.
//! - **Jump bookkeeping**: the `__baregen_jump!` markers embedded in a
//!   block's statements correspond one-to-one to its `jumps` list, and
//!   every jump entry is owned by exactly one block.
//! - **Liveness consistency**: the analysis' `live_in` sets are an exact
//!   fixed point of the backward dataflow equations over the final CFG.
//! - **Def-use consistency**: every binding live at a block entry (in
//!   particular, every variant field) is initialized on all paths from
//!   the entry; at the function entry only arguments may be live.
//! - **Variable transfer**: a variant's fields are exactly the storable
//!   live-in bindings (no omissions, no duplicates, no leftover direct
//!   borrows), reborrows are rebuilt from names available in the arm,
//!   and no transition (terminator edge or opaque jump) would capture a
//!   same-named shadowing binding instead of the live one it moves.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use quote::ToTokens;

use crate::analyze_cfg::Analysis;
use crate::cfg::{BindingId, BlockId, BorrowSource, Cfg, OpaqueJumpKind, Terminator};
use crate::lower::collect_markers;

/// Validates a lowered CFG against its analysis. `n_args` is the number
/// of function arguments (`BindingId(0)..BindingId(n_args)`), which are
/// the only bindings defined at the function entry.
pub fn validate(cfg: &Cfg, analysis: &Analysis, n_args: usize) -> Result<(), String> {
    let mut v = Validator {
        cfg,
        analysis,
        n_args,
        violations: Vec::new(),
    };
    // Index validity and analysis shape first: the remaining checks
    // index blocks and bindings freely, so bail out if these fail.
    v.check_edge_targets();
    v.check_analysis_shape();
    if !v.violations.is_empty() {
        return v.finish();
    }
    let preds = v.pred_edges();
    v.check_reachability();
    v.check_inlining(&preds);
    v.check_resume_points(&preds);
    let marker_pos = v.check_jump_markers();
    v.check_variant_list();
    v.check_liveness_equations();
    v.check_definedness(&preds);
    v.check_variant_fields();
    v.check_transition_shadowing(&marker_pos);
    v.finish()
}

struct Validator<'a> {
    cfg: &'a Cfg,
    analysis: &'a Analysis,
    n_args: usize,
    violations: Vec<String>,
}

impl Validator<'_> {
    fn report(&mut self, msg: String) {
        self.violations.push(msg);
    }

    fn finish(self) -> Result<(), String> {
        if self.violations.is_empty() {
            Ok(())
        } else {
            let list: Vec<String> = self
                .violations
                .iter()
                .enumerate()
                .map(|(i, v)| format!("  {}. {v}", i + 1))
                .collect();
            Err(list.join("\n"))
        }
    }

    /// `block {b}`, with the variant name when it has one.
    fn block_desc(&self, b: BlockId) -> String {
        match self.analysis.variant(b) {
            Some(v) => format!("block {b} (state `{}`)", v.ident),
            None => format!("block {b}"),
        }
    }

    fn binding_desc(&self, id: BindingId) -> String {
        format!("`{}` (binding {})", self.cfg.bindings[id.0].ident, id.0)
    }

    fn names(&self, ids: impl IntoIterator<Item = BindingId>) -> String {
        let v: Vec<String> = ids.into_iter().map(|id| self.binding_desc(id)).collect();
        v.join(", ")
    }

    // === CFG well-formedness ===

    fn check_edge_targets(&mut self) {
        let n = self.cfg.blocks.len();
        if self.cfg.entry >= n {
            self.report(format!(
                "entry block {} does not exist ({n} blocks)",
                self.cfg.entry
            ));
        }
        for (b, blk) in self.cfg.blocks.iter().enumerate() {
            for s in blk.terminator.successors() {
                if s >= n {
                    self.report(format!(
                        "the terminator of block {b} targets nonexistent block {s} ({n} blocks)"
                    ));
                }
            }
            for &k in &blk.jumps {
                if k >= self.cfg.opaque_jumps.len() {
                    self.report(format!(
                        "block {b} owns nonexistent opaque jump {k} ({} jumps)",
                        self.cfg.opaque_jumps.len()
                    ));
                } else if let OpaqueJumpKind::Goto { target, .. } = self.cfg.opaque_jumps[k].kind
                    && target >= n
                {
                    self.report(format!(
                        "opaque jump {k} in block {b} targets nonexistent block {target} \
                         ({n} blocks)"
                    ));
                }
            }
        }
    }

    fn check_analysis_shape(&mut self) {
        let n = self.cfg.blocks.len();
        for (name, len) in [
            ("live_in", self.analysis.live_in.len()),
            ("uses", self.analysis.uses.len()),
            ("removed_stmts", self.analysis.removed_stmts.len()),
        ] {
            if len != n {
                self.report(format!(
                    "analysis `{name}` covers {len} blocks but the CFG has {n}"
                ));
            }
        }
        if self.analysis.removed_stmts.len() == n {
            for (b, removed) in self.analysis.removed_stmts.iter().enumerate() {
                for &i in removed {
                    match self.cfg.blocks[b].stmts.get(i) {
                        None => self.report(format!(
                            "removed statement {i} of block {b} does not exist \
                             ({} statements)",
                            self.cfg.blocks[b].stmts.len()
                        )),
                        Some(syn::Stmt::Local(_)) => {}
                        Some(_) => self.report(format!(
                            "removed statement {i} of block {b} is not a `let` \
                             (only original borrow `let`s may be removed)"
                        )),
                    }
                }
            }
        }
        for id in self
            .analysis
            .live_in
            .iter()
            .chain(self.analysis.uses.iter())
            .flatten()
        {
            if id.0 >= self.cfg.bindings.len() {
                self.report(format!(
                    "analysis mentions nonexistent binding {} ({} bindings)",
                    id.0,
                    self.cfg.bindings.len()
                ));
            }
        }
        for id in self
            .cfg
            .blocks
            .iter()
            .flat_map(|b| b.uses.iter().chain(b.defs.iter()))
        {
            if id.0 >= self.cfg.bindings.len() {
                self.report(format!(
                    "the CFG mentions nonexistent binding {} ({} bindings)",
                    id.0,
                    self.cfg.bindings.len()
                ));
            }
        }
    }

    /// Predecessor edges of every block, opaque-jump edges included
    /// (parallel edges kept: an `if` with identical arms counts twice).
    fn pred_edges(&self) -> Vec<Vec<BlockId>> {
        let mut preds = vec![Vec::new(); self.cfg.blocks.len()];
        for b in 0..self.cfg.blocks.len() {
            for s in self.cfg.successors(b) {
                preds[s].push(b);
            }
        }
        preds
    }

    fn check_reachability(&mut self) {
        let mut reachable = vec![false; self.cfg.blocks.len()];
        let mut stack = vec![self.cfg.entry];
        while let Some(b) = stack.pop() {
            if std::mem::replace(&mut reachable[b], true) {
                continue;
            }
            stack.extend(self.cfg.successors(b));
        }
        for (b, r) in reachable.iter().enumerate() {
            if !r {
                self.report(format!(
                    "{} is unreachable from the entry (simplification should have \
                     removed it)",
                    self.block_desc(b)
                ));
            }
        }
    }

    /// Inline blocks are emitted inside their predecessor's arm, so they
    /// must have exactly one predecessor and must not be entered by
    /// dispatch (entry, resume, or opaque jump).
    fn check_inlining(&mut self, preds: &[Vec<BlockId>]) {
        for (b, p) in preds.iter().enumerate() {
            if !self.cfg.blocks[b].inline {
                continue;
            }
            if b == self.cfg.entry {
                self.report("the entry block is marked inline".to_string());
            }
            if self.cfg.blocks[b].resume_point {
                self.report(format!("resume-point block {b} is marked inline"));
            }
            if p.len() != 1 {
                self.report(format!(
                    "inline block {b} has {} predecessors (must have exactly 1)",
                    p.len()
                ));
            }
        }
        for (b, blk) in self.cfg.blocks.iter().enumerate() {
            for &k in &blk.jumps {
                if let OpaqueJumpKind::Goto { target, .. } = self.cfg.opaque_jumps[k].kind
                    && self.cfg.blocks[target].inline
                {
                    self.report(format!(
                        "opaque jump {k} in block {b} targets inline block {target}, \
                         which has no dispatchable state"
                    ));
                }
            }
        }
    }

    /// Every `Yield` continues into a resume point, and every resume
    /// point is entered only by its yield: the generated
    /// `__resume.take().expect(..)` relies on the resume arm running at
    /// most once per `__drive` call.
    fn check_resume_points(&mut self, preds: &[Vec<BlockId>]) {
        let mut yield_preds = vec![0usize; self.cfg.blocks.len()];
        for (b, blk) in self.cfg.blocks.iter().enumerate() {
            if let Terminator::Yield { next, .. } = blk.terminator {
                yield_preds[next] += 1;
                if !self.cfg.blocks[next].resume_point {
                    self.report(format!(
                        "the yield in block {b} continues into {}, which is not a \
                         resume point",
                        self.block_desc(next)
                    ));
                }
            }
        }
        for b in 0..self.cfg.blocks.len() {
            if self.cfg.blocks[b].resume_point && (preds[b].len() != 1 || yield_preds[b] != 1) {
                self.report(format!(
                    "resume point {} must have its yield as its only predecessor, \
                     but has {} predecessors ({} of them yields)",
                    self.block_desc(b),
                    preds[b].len(),
                    yield_preds[b]
                ));
            }
        }
    }

    /// The `__baregen_jump!(k, ..)` markers embedded in a block's
    /// statement tokens must match its `jumps` list exactly, and each
    /// jump entry must be owned by exactly one block. Returns each
    /// marker's position (block, statement index) for the shadowing
    /// check.
    fn check_jump_markers(&mut self) -> HashMap<usize, (BlockId, usize)> {
        let mut owner: HashMap<usize, BlockId> = HashMap::new();
        let mut positions: HashMap<usize, (BlockId, usize)> = HashMap::new();
        for (b, blk) in self.cfg.blocks.iter().enumerate() {
            for &k in &blk.jumps {
                if let Some(prev) = owner.insert(k, b) {
                    self.report(format!(
                        "opaque jump {k} is owned by both block {prev} and block {b}"
                    ));
                }
            }
            let mut found: Vec<usize> = Vec::new();
            for (i, stmt) in blk.stmts.iter().enumerate() {
                let mut ks = Vec::new();
                collect_markers(stmt.to_token_stream(), &mut ks);
                for k in ks {
                    positions.entry(k).or_insert((b, i));
                    found.push(k);
                }
            }
            let mut listed: Vec<usize> = blk.jumps.clone();
            found.sort_unstable();
            listed.sort_unstable();
            if found != listed {
                self.report(format!(
                    "jump markers embedded in block {b} ({found:?}) do not match its \
                     `jumps` list ({listed:?})"
                ));
            }
        }
        positions
    }

    // === Dispatch totality ===

    /// One variant (= dispatch arm) per non-inline block, in ascending
    /// block order, with unique names, `Start` at the entry.
    fn check_variant_list(&mut self) {
        let mut names: BTreeMap<String, BlockId> = BTreeMap::new();
        let mut prev: Option<BlockId> = None;
        for v in &self.analysis.variants {
            if v.block >= self.cfg.blocks.len() {
                self.report(format!(
                    "variant `{}` refers to nonexistent block {}",
                    v.ident, v.block
                ));
                continue;
            }
            if let Some(p) = prev
                && v.block <= p
            {
                self.report(format!(
                    "variant list is not in strictly ascending block order at `{}` \
                     (block {} after block {p})",
                    v.ident, v.block
                ));
            }
            prev = Some(v.block);
            if self.cfg.blocks[v.block].inline {
                self.report(format!(
                    "inline block {} has a variant `{}` (inline blocks must not \
                     become states)",
                    v.block, v.ident
                ));
            }
            if let Some(other) = names.insert(v.ident.to_string(), v.block) {
                self.report(format!(
                    "two states share the name `{}` (blocks {other} and {})",
                    v.ident, v.block
                ));
            }
        }
        for b in 0..self.cfg.blocks.len() {
            if !self.cfg.blocks[b].inline && self.analysis.variant(b).is_none() {
                self.report(format!(
                    "non-inline block {b} has no state variant: the dispatch loop \
                     could never enter it"
                ));
            }
        }
        if let Some(v) = self.analysis.variant(self.cfg.entry)
            && v.ident != "Start"
        {
            self.report(format!(
                "the entry block's variant is named `{}` instead of `Start`",
                v.ident
            ));
        }
    }

    // === Liveness and def-use consistency ===

    /// `live_in` must be an exact fixed point of
    /// `live_in(B) = use(B) ∪ (⋃ live_in(succ) ∖ def(B))`
    /// over the final CFG (opaque-jump edges included).
    fn check_liveness_equations(&mut self) {
        for b in 0..self.cfg.blocks.len() {
            let mut expected = BTreeSet::new();
            for s in self.cfg.successors(b) {
                expected.extend(self.analysis.live_in[s].iter().copied());
            }
            for d in &self.cfg.blocks[b].defs {
                expected.remove(d);
            }
            expected.extend(self.analysis.uses[b].iter().copied());
            let actual = &self.analysis.live_in[b];
            if expected != *actual {
                let missing: Vec<BindingId> = expected.difference(actual).copied().collect();
                let extra: Vec<BindingId> = actual.difference(&expected).copied().collect();
                let mut parts = Vec::new();
                if !missing.is_empty() {
                    parts.push(format!("missing {}", self.names(missing)));
                }
                if !extra.is_empty() {
                    parts.push(format!("spurious {}", self.names(extra)));
                }
                self.report(format!(
                    "liveness equation violated at {}: live-in should be \
                     use ∪ (successors' live-in ∖ def) but is {}",
                    self.block_desc(b),
                    parts.join("; ")
                ));
            }
        }
    }

    /// Forward must-initialization: every binding live at a block entry
    /// (in particular, every variant field) is defined on all paths from
    /// the entry, where only the arguments are defined.
    fn check_definedness(&mut self, preds: &[Vec<BlockId>]) {
        let n = self.cfg.blocks.len();
        let universe: BTreeSet<BindingId> = (0..self.cfg.bindings.len()).map(BindingId).collect();
        let args: BTreeSet<BindingId> = (0..self.n_args).map(BindingId).collect();
        let mut defined: Vec<BTreeSet<BindingId>> = vec![universe; n];
        defined[self.cfg.entry] = args;
        loop {
            let mut changed = false;
            for b in 0..n {
                if b == self.cfg.entry {
                    continue;
                }
                let mut inter: Option<BTreeSet<BindingId>> = None;
                for &p in &preds[b] {
                    let mut out = defined[p].clone();
                    out.extend(self.cfg.blocks[p].defs.iter().copied());
                    inter = Some(match inter {
                        None => out,
                        Some(cur) => cur.intersection(&out).copied().collect(),
                    });
                }
                let new = inter.unwrap_or_else(|| defined[b].clone());
                if new != defined[b] {
                    defined[b] = new;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for (b, defined) in defined.iter().enumerate() {
            let uninit: Vec<BindingId> = self.analysis.live_in[b]
                .difference(defined)
                .copied()
                .collect();
            if !uninit.is_empty() {
                self.report(format!(
                    "{} live at the entry of {} but not initialized on every path \
                     from the function entry",
                    self.names(uninit),
                    self.block_desc(b)
                ));
            }
        }
    }

    // === Variable transfer ===

    /// A variant's fields are exactly the storable live-in bindings of
    /// its block: one field per binding, named after it, no duplicates,
    /// and direct borrows (substituted by their sources) never appear.
    /// Reborrows must be rebuildable from names available in the arm.
    fn check_variant_fields(&mut self) {
        let mut def_anywhere = vec![false; self.cfg.bindings.len()];
        for blk in &self.cfg.blocks {
            for d in &blk.defs {
                def_anywhere[d.0] = true;
            }
        }
        for v in &self.analysis.variants {
            let b = v.block;
            let mut expected: BTreeMap<String, usize> = BTreeMap::new();
            for id in &self.analysis.live_in[b] {
                let binding = &self.cfg.bindings[id.0];
                if matches!(binding.borrow, BorrowSource::Direct { .. }) {
                    if def_anywhere[id.0] {
                        self.report(format!(
                            "direct borrow {} is live at the entry of {}; it should \
                             have been substituted by its source",
                            self.binding_desc(*id),
                            self.block_desc(b)
                        ));
                    }
                    continue;
                }
                *expected.entry(binding.ident.to_string()).or_insert(0) += 1;
            }
            for (name, count) in &expected {
                if *count > 1 {
                    self.report(format!(
                        "{count} distinct live bindings named `{name}` at the entry \
                         of {} would collide in one state",
                        self.block_desc(b)
                    ));
                }
            }
            let mut actual: BTreeMap<String, usize> = BTreeMap::new();
            for f in &v.fields {
                *actual.entry(f.ident.to_string()).or_insert(0) += 1;
            }
            for (name, count) in &actual {
                if *count > 1 {
                    self.report(format!(
                        "state `{}` has {count} fields named `{name}`",
                        v.ident
                    ));
                }
            }
            for name in expected.keys() {
                if !actual.contains_key(name) {
                    self.report(format!(
                        "state `{}` stores no field for the live binding `{name}` \
                         (the value would be lost at the transition)",
                        v.ident
                    ));
                }
            }
            for name in actual.keys() {
                if !expected.contains_key(name) {
                    self.report(format!(
                        "state `{}` has a field `{name}` with no live binding of \
                         that name at its entry",
                        v.ident
                    ));
                }
            }
            // Reborrows: each source must be available at the arm head —
            // a field of this variant, an earlier reborrow target, or
            // (in the entry arm, which unpacks every argument) an
            // argument name.
            let mut available: BTreeSet<String> = actual.keys().cloned().collect();
            if b == self.cfg.entry {
                available.extend((0..self.n_args).map(|i| self.cfg.bindings[i].ident.to_string()));
            }
            for rb in &v.reborrows {
                if !available.contains(&rb.source.to_string()) {
                    self.report(format!(
                        "the reborrow `{} = &{}` at the head of {} has no source: \
                         `{}` is neither a field nor an earlier reborrow",
                        rb.target,
                        rb.source,
                        self.block_desc(b),
                        rb.source
                    ));
                }
                available.insert(rb.target.to_string());
            }
        }
    }

    /// Transitions move a target's fields by name from the transition
    /// site. A binding defined between the arm's variant unpack and the
    /// transition that shares its name with a (different) live binding
    /// being moved would be captured instead — a silent wrong-value
    /// miscompile. Terminator transitions sit at the end of the inline
    /// chain; opaque jumps fire inside a statement, so only definitions
    /// ordered before that statement count (where the order is known).
    fn check_transition_shadowing(&mut self, marker_pos: &HashMap<usize, (BlockId, usize)>) {
        let n = self.cfg.blocks.len();
        // Unique terminator predecessor, for walking inline chains.
        let mut term_pred = vec![usize::MAX; n];
        for b in 0..n {
            for s in self.cfg.blocks[b].terminator.successors() {
                term_pred[s] = b;
            }
        }
        // The inline chain ending at `c`: c itself, then its unique
        // predecessors while they are inline, ending at the region root
        // whose arm textually contains all of them.
        let chain = |c: BlockId| -> Vec<BlockId> {
            let mut v = vec![c];
            let mut cur = c;
            while self.cfg.blocks[cur].inline && term_pred[cur] != usize::MAX && v.len() <= n {
                cur = term_pred[cur];
                v.push(cur);
            }
            v
        };
        for c in 0..n {
            let chain_blocks = chain(c);
            let all_defs: BTreeSet<BindingId> = chain_blocks
                .iter()
                .flat_map(|&d| self.cfg.blocks[d].defs.iter().copied())
                .collect();
            // Terminator edges: every definition in the chain textually
            // precedes the transition at the end of `c`.
            let shadow: BTreeSet<BindingId> = chain_blocks
                .iter()
                .flat_map(|&d| self.shadowing_defs(d))
                .collect();
            for t in self.cfg.blocks[c].terminator.successors() {
                if !self.cfg.blocks[t].inline {
                    self.check_shadowed_transfer(c, t, &all_defs, &shadow, "transition");
                }
            }
            // Opaque jumps: definitions in `c` after the jump-carrying
            // statement do not shadow the jump site.
            for &k in &self.cfg.blocks[c].jumps {
                let OpaqueJumpKind::Goto { target, .. } = self.cfg.opaque_jumps[k].kind else {
                    continue;
                };
                let marker_stmt = marker_pos.get(&k).map(|&(_, i)| i);
                let mut shadow: BTreeSet<BindingId> = chain_blocks[1..]
                    .iter()
                    .flat_map(|&d| self.shadowing_defs(d))
                    .collect();
                for id in self.shadowing_defs(c) {
                    // A definition at a known statement index after the
                    // marker is not in scope at the jump; anything with
                    // an unknown position is counted conservatively.
                    let before = match (self.cfg.bindings[id.0].def_stmt, marker_stmt) {
                        (Some(d), Some(m)) => d < m,
                        _ => true,
                    };
                    if before {
                        shadow.insert(id);
                    }
                }
                self.check_shadowed_transfer(c, target, &all_defs, &shadow, "opaque jump");
            }
        }
    }

    /// The definitions of `d` that shadow names in its emitted arm.
    /// Excluded: bindings introduced by the braces-scoped `let`
    /// synthesized for a valued opaque break (never in scope outside the
    /// marker's own braces), and bindings whose original borrow `let`
    /// the analysis removed from codegen (not emitted at all).
    fn shadowing_defs(&self, d: BlockId) -> Vec<BindingId> {
        let stores: BTreeSet<BindingId> = self.cfg.blocks[d]
            .jumps
            .iter()
            .filter_map(|&k| match self.cfg.opaque_jumps[k].kind {
                OpaqueJumpKind::Goto { store: Some(s), .. } => Some(s),
                _ => None,
            })
            .collect();
        self.cfg.blocks[d]
            .defs
            .iter()
            .copied()
            .filter(|id| !stores.contains(id))
            .filter(|id| {
                !self.cfg.bindings[id.0]
                    .def_stmt
                    .is_some_and(|i| self.analysis.removed_stmts[d].contains(&i))
            })
            .collect()
    }

    /// One transfer edge into non-inline `t`: flags a live binding of
    /// `t` that is not defined anywhere in the chain (it comes from the
    /// arm's variant unpack) while a different same-named binding is.
    fn check_shadowed_transfer(
        &mut self,
        c: BlockId,
        t: BlockId,
        chain_defs: &BTreeSet<BindingId>,
        shadow: &BTreeSet<BindingId>,
        kind: &str,
    ) {
        for &id in &self.analysis.live_in[t] {
            if chain_defs.contains(&id) {
                continue;
            }
            let ident = &self.cfg.bindings[id.0].ident;
            for &other in shadow {
                if other != id && self.cfg.bindings[other.0].ident == *ident {
                    self.report(format!(
                        "the {kind} from {} to {} moves `{ident}` by name, but a \
                         different binding also named `{ident}` (binding {}) is in \
                         scope at the transition and would be captured instead of \
                         {}",
                        self.block_desc(c),
                        self.block_desc(t),
                        other.0,
                        self.binding_desc(id)
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
