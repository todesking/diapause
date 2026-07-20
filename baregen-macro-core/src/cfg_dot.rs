//! Graphviz DOT export of a [`Cfg`] for debugging front ends (the
//! playground renders the string client-side with a wasm build of
//! Graphviz). Works on both the unsimplified and the simplified CFG;
//! when an [`Analysis`] is supplied, blocks are additionally annotated
//! with their state-variant names.

use std::fmt::Write;

use quote::ToTokens;

use crate::analyze_cfg::Analysis;
use crate::cfg::{Cfg, OpaqueJumpKind, Terminator};

/// Longest rendered statement/expression line; the rest is elided so a
/// single long opaque statement cannot blow up the node width.
const MAX_LINE_CHARS: usize = 60;

/// Renders `cfg` as a Graphviz `digraph`.
///
/// - Each block is a box listing its id and statements, plus a summary
///   line for value-carrying terminators (`if c` / `match s` /
///   `yield v` / `it.next()` / `return e`).
/// - Terminator edges are labeled with the branch they take (`then` /
///   `else`, match-arm patterns, `resume [binding]`, `Some(pat)` /
///   `None`).
/// - Resume points (blocks that become resumable state variants) get a
///   double border; the entry block and inlined blocks are tagged in
///   their title. With `analysis`, variant names appear as `[Name]`.
/// - Opaque jumps (`break`/`continue` markers embedded in opaque
///   statements) become dashed edges; a completing jump points at a
///   synthetic `complete` node.
pub fn cfg_to_dot(cfg: &Cfg, analysis: Option<&Analysis>) -> String {
    let mut out = String::new();
    out.push_str("digraph cfg {\n");
    out.push_str("    rankdir=TB;\n");
    out.push_str("    node [shape=box, fontname=\"monospace\"];\n");
    for (i, _) in cfg.blocks.iter().enumerate() {
        write_node(&mut out, cfg, analysis, i);
    }
    if has_complete_jump(cfg) {
        out.push_str("    complete [shape=ellipse, label=\"complete\"];\n");
    }
    for (i, _) in cfg.blocks.iter().enumerate() {
        write_edges(&mut out, cfg, i);
    }
    out.push_str("}\n");
    out
}

fn write_node(out: &mut String, cfg: &Cfg, analysis: Option<&Analysis>, i: usize) {
    let block = &cfg.blocks[i];
    let mut lines = vec![title(cfg, analysis, i)];
    for stmt in &block.stmts {
        lines.push(tokens_line(stmt));
    }
    if let Some(term) = terminator_line(&block.terminator) {
        lines.push(term);
    }
    let label: String = lines.iter().map(|l| format!("{}\\l", escape(l))).collect();
    let mut attrs = format!("label=\"{label}\"");
    if block.resume_point {
        attrs.push_str(", peripheries=2");
    }
    if block.inline {
        attrs.push_str(", style=dashed");
    }
    let _ = writeln!(out, "    b{i} [{attrs}];");
}

/// The node's first line: block id, variant name (with `analysis`), and
/// role markers.
fn title(cfg: &Cfg, analysis: Option<&Analysis>, i: usize) -> String {
    let mut t = format!("b{i}");
    if let Some(v) = analysis.and_then(|a| a.variant(i)) {
        let _ = write!(t, " [{}]", v.ident);
    }
    if i == cfg.entry {
        t.push_str(" (entry)");
    }
    if cfg.blocks[i].resume_point {
        t.push_str(" (resume)");
    }
    if cfg.blocks[i].inline {
        t.push_str(" (inline)");
    }
    t
}

/// Summary of the terminator's own expression, shown as the node's last
/// line. `Goto` carries no expression and gets no line: the edge alone
/// says everything.
fn terminator_line(term: &Terminator) -> Option<String> {
    match term {
        Terminator::Goto(_) => None,
        Terminator::Branch { cond, .. } => Some(format!("if {}", tokens_line(cond))),
        Terminator::Match { scrutinee, .. } => Some(format!("match {}", tokens_line(scrutinee))),
        Terminator::Yield { value, .. } => Some(format!("yield {}", tokens_line(value))),
        Terminator::IterNext { iter, .. } => Some(format!("{iter}.next()")),
        Terminator::Return(e) => Some(format!("return {}", tokens_line(e))),
    }
}

fn write_edges(out: &mut String, cfg: &Cfg, i: usize) {
    match &cfg.blocks[i].terminator {
        Terminator::Goto(t) => edge(out, i, *t, "", false),
        Terminator::Branch { then_, else_, .. } => {
            edge(out, i, *then_, "then", false);
            edge(out, i, *else_, "else", false);
        }
        Terminator::Match { arms, .. } => {
            for arm in arms {
                let mut label = tokens_line(&arm.pat);
                if let Some(guard) = &arm.guard {
                    let _ = write!(label, " if {}", tokens_line(guard));
                }
                edge(out, i, arm.body, &label, false);
            }
        }
        Terminator::Yield {
            resume_binding,
            next,
            ..
        } => {
            let label = match resume_binding {
                Some(rb) => format!("resume {}", cfg.bindings[rb.binding.0].ident),
                None => "resume".to_string(),
            };
            edge(out, i, *next, &label, false);
        }
        Terminator::IterNext {
            pat, body, exit, ..
        } => {
            edge(out, i, *body, &format!("Some({})", tokens_line(pat)), false);
            edge(out, i, *exit, "None", false);
        }
        Terminator::Return(_) => {}
    }
    for &j in &cfg.blocks[i].jumps {
        match cfg.opaque_jumps[j].kind {
            OpaqueJumpKind::Goto { target, store } => {
                let label = match store {
                    Some(b) => format!("jump (store {})", cfg.bindings[b.0].ident),
                    None => "jump".to_string(),
                };
                edge(out, i, target, &label, true);
            }
            OpaqueJumpKind::Complete => {
                let _ = writeln!(out, "    b{i} -> complete [style=dashed, label=\"jump\"];");
            }
        }
    }
}

fn edge(out: &mut String, from: usize, to: usize, label: &str, dashed: bool) {
    let mut attrs = String::new();
    if !label.is_empty() {
        let _ = write!(attrs, "label=\"{}\"", escape(label));
    }
    if dashed {
        if !attrs.is_empty() {
            attrs.push_str(", ");
        }
        attrs.push_str("style=dashed");
    }
    if attrs.is_empty() {
        let _ = writeln!(out, "    b{from} -> b{to};");
    } else {
        let _ = writeln!(out, "    b{from} -> b{to} [{attrs}];");
    }
}

fn has_complete_jump(cfg: &Cfg) -> bool {
    cfg.blocks
        .iter()
        .flat_map(|b| b.jumps.iter())
        .any(|&j| matches!(cfg.opaque_jumps[j].kind, OpaqueJumpKind::Complete))
}

/// One display line for a syntax node: its token string, elided past
/// [`MAX_LINE_CHARS`].
fn tokens_line<T: ToTokens>(node: &T) -> String {
    let s = node.to_token_stream().to_string();
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(MAX_LINE_CHARS).collect();
    if chars.next().is_some() {
        head + "…"
    } else {
        head
    }
}

/// Escapes a line for use inside a double-quoted DOT label.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests;
