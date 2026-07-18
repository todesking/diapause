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

use std::collections::{BTreeMap, HashMap};

use proc_macro2::{TokenStream, TokenTree};
use quote::ToTokens;

use crate::analyze_cfg::Analysis;
use crate::cfg::{BlockId, Cfg, OpaqueJumpKind, Terminator};

/// Validates a lowered CFG against its analysis. `n_args` is the number
/// of function arguments (`BindingId(0)..BindingId(n_args)`), which are
/// the only bindings defined at the function entry (reserved for the
/// definite-initialization checks).
pub fn validate(cfg: &Cfg, analysis: &Analysis, _n_args: usize) -> Result<(), String> {
    let mut v = Validator {
        cfg,
        analysis,
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
    v.check_jump_markers();
    v.check_variant_list();
    v.finish()
}

struct Validator<'a> {
    cfg: &'a Cfg,
    analysis: &'a Analysis,
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
        for id in self.analysis.live_in.iter().flatten() {
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
    /// jump entry must be owned by exactly one block.
    fn check_jump_markers(&mut self) {
        let mut owner: HashMap<usize, BlockId> = HashMap::new();
        for (b, blk) in self.cfg.blocks.iter().enumerate() {
            for &k in &blk.jumps {
                if let Some(prev) = owner.insert(k, b) {
                    self.report(format!(
                        "opaque jump {k} is owned by both block {prev} and block {b}"
                    ));
                }
            }
            let mut found: Vec<usize> = Vec::new();
            for stmt in &blk.stmts {
                collect_markers(stmt.to_token_stream(), &mut found);
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
}

/// Collects the `k` arguments of every `__baregen_jump!(k [, value])`
/// marker in a token stream, including markers nested inside another
/// marker's value.
fn collect_markers(tokens: TokenStream, out: &mut Vec<usize>) {
    let mut iter = tokens.into_iter().peekable();
    while let Some(tt) = iter.next() {
        match tt {
            TokenTree::Ident(id) if id == "__baregen_jump" => {
                if !matches!(iter.peek(), Some(TokenTree::Punct(p)) if p.as_char() == '!') {
                    continue;
                }
                iter.next(); // the `!`
                if let Some(TokenTree::Group(g)) = iter.next() {
                    let mut inner = g.stream().into_iter();
                    if let Some(TokenTree::Literal(l)) = inner.next()
                        && let Ok(k) = l.to_string().parse::<usize>()
                    {
                        out.push(k);
                    }
                    // A completion marker's value may contain further
                    // (already rewritten) markers.
                    collect_markers(inner.collect(), out);
                }
            }
            TokenTree::Group(g) => collect_markers(g.stream(), out),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;
