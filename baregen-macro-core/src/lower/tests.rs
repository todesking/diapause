use super::*;
use crate::cfg::OpaqueJumpKind;
use crate::test_util::lower_args;
use syn::parse_quote;

fn lower_ok(block: &syn::Block) -> Cfg {
    lower_args(&[], block)
}

fn error_of(block: &syn::Block) -> syn::Error {
    lower(&[], &[], block).unwrap_err()
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
        Terminator::Branch {
            then_: 2,
            else_: 1,
            ..
        }
    ));
    let then_ = &cfg.blocks[2];
    assert!(then_.inline && !then_.resume_point);
    assert!(then_.stmts.is_empty());
    assert!(matches!(
        then_.terminator,
        Terminator::Yield { next: 3, .. }
    ));
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
        panic!(
            "if let should lower to a match: {:?}",
            cfg.blocks[0].terminator
        );
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
        panic!(
            "let-else should lower to a match: {:?}",
            cfg.blocks[0].terminator
        );
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
    let Terminator::IterNext {
        iter, body, exit, ..
    } = &hb.terminator
    else {
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
fn inclusive_for_head_uses_the_exhaustion_preserving_wrapper() {
    let block: syn::Block = parse_quote!({
        for i in 0u32..=n {
            yield_!(i);
        }
    });
    let cfg = lower_args(&["n"], &block);
    let it = binding(&cfg, "__iter0");
    assert!(matches!(
        &cfg.bindings[it.0].ty,
        TySource::RangeInclusiveIter(inner)
            if matches!(**inner, TySource::Range { inclusive: true, .. })
    ));
    let expected: syn::Stmt = parse_quote! {
        let mut __iter0 = __RangeInclusiveIter::new(0u32..=n);
    };
    assert_eq!(cfg.blocks[0].stmts[0], expected);
}

#[test]
fn moved_inclusive_range_head_is_wrapped_too() {
    let block: syn::Block = parse_quote!({
        let r = 0u32..=n;
        let s = r;
        for i in s {
            yield_!(i);
        }
    });
    let cfg = lower_args(&["n"], &block);
    let it = binding(&cfg, "__iter0");
    assert!(matches!(
        &cfg.bindings[it.0].ty,
        TySource::RangeInclusiveIter(inner) if matches!(**inner, TySource::Moved(_))
    ));
    let expected: syn::Stmt = parse_quote! {
        let mut __iter0 = __RangeInclusiveIter::new(s);
    };
    assert_eq!(cfg.blocks[0].stmts[2], expected);
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
    let cfg = lower_args(&[], &block);
    assert!(cfg.opaque_jumps.is_empty());
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
    let n_defs = cfg
        .blocks
        .iter()
        .filter(|blk| blk.defs.contains(&x))
        .count();
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
    assert!(
        returns
            .iter()
            .any(|t| matches!(t, Terminator::Return(e) if *e == one))
    );
    assert!(
        returns
            .iter()
            .any(|t| matches!(t, Terminator::Return(e) if *e == two))
    );
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
    // Hoisting happens in a pre-pass; by the time an expression
    // yield reaches lowering it is unhoistable.
    let block: syn::Block = parse_quote!({
        f(1, yield_!(2));
    });
    let err = error_of(&block);
    assert!(err.to_string().contains("path, a literal"));
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
    let block: syn::Block = parse_quote!({ yield_!(1) });
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
        if let Some(x) = opt
            && c
        {
            yield_!(x);
        }
    });
    assert!(error_of(&block).to_string().contains("let-chain"));
    let block: syn::Block = parse_quote!({
        while let Some(x) = opt
            && c
        {
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
    assert!(error_of(&block).to_string().contains("iterator expression"));
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
    assert!(lower(&idents, &[], &block).is_ok());
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

/// The single block owning a jump marker, with its jump indices.
fn jump_owner(cfg: &Cfg) -> &Block {
    let owners: Vec<&Block> = cfg.blocks.iter().filter(|b| !b.jumps.is_empty()).collect();
    assert_eq!(owners.len(), 1, "expected exactly one jump-owning block");
    owners[0]
}

fn stmt_text(b: &Block) -> String {
    let stmts = &b.stmts;
    quote::quote!(#(#stmts)*).to_string()
}

#[test]
fn opaque_break_into_expanded_loop_is_rewritten() {
    let block: syn::Block = parse_quote!({
        loop {
            yield_!(1);
            if c {
                break;
            }
        }
        after();
    });
    let cfg = lower_ok(&block);
    assert_eq!(cfg.opaque_jumps.len(), 1);
    let OpaqueJumpKind::Goto {
        target,
        store: None,
    } = cfg.opaque_jumps[0].kind
    else {
        panic!("expected a plain goto jump: {:?}", cfg.opaque_jumps[0]);
    };
    // The target is the loop's exit block, reachable only through
    // the jump edge, and kept a variant.
    let exit = &cfg.blocks[target];
    assert!(!exit.inline, "jump targets must stay variants");
    assert_eq!(stmt_text(exit), "after () ;");
    // The rewritten statement carries the marker; the block records
    // the edge.
    let owner = jump_owner(&cfg);
    assert_eq!(owner.jumps, vec![0]);
    assert!(stmt_text(owner).contains("__baregen_jump ! (0)"));
    assert!(matches!(owner.terminator, Terminator::Goto(_)));
}

#[test]
fn opaque_labeled_jumps_into_expanded_loop_are_rewritten() {
    let block: syn::Block = parse_quote!({
        'a: loop {
            yield_!(1);
            loop {
                break 'a;
            }
        }
        after();
    });
    let cfg = lower_ok(&block);
    assert_eq!(cfg.opaque_jumps.len(), 1);
    let OpaqueJumpKind::Goto { target, .. } = cfg.opaque_jumps[0].kind else {
        panic!("expected a goto jump");
    };
    assert_eq!(stmt_text(&cfg.blocks[target]), "after () ;");

    let block: syn::Block = parse_quote!({
        'a: loop {
            yield_!(1);
            loop {
                if c {
                    continue 'a;
                }
                break;
            }
        }
    });
    let cfg = lower_ok(&block);
    assert_eq!(cfg.opaque_jumps.len(), 1);
    let OpaqueJumpKind::Goto { target, .. } = cfg.opaque_jumps[0].kind else {
        panic!("expected a goto jump");
    };
    // `continue 'a` targets the expanded loop's header (the block
    // yielding 1); the inner loop's own `break` stays untouched.
    assert!(matches!(
        cfg.blocks[target].terminator,
        Terminator::Yield { .. }
    ));
    assert!(stmt_text(jump_owner(&cfg)).contains("break ;"));
}

#[test]
fn opaque_valued_break_into_let_initializer_loop_stores_and_jumps() {
    let block: syn::Block = parse_quote!({
        let x: u32 = loop {
            let r = yield_!(1);
            if r > 3 {
                break r;
            }
        };
        f(x);
    });
    let cfg = lower_ok(&block);
    assert_eq!(cfg.opaque_jumps.len(), 1);
    let OpaqueJumpKind::Goto {
        target,
        store: Some(s),
    } = cfg.opaque_jumps[0].kind
    else {
        panic!("expected a storing goto jump: {:?}", cfg.opaque_jumps[0]);
    };
    assert_eq!(s, binding(&cfg, "x"));
    assert_eq!(stmt_text(&cfg.blocks[target]), "f (x) ;");
    // The jump site binds the destination with a synthesized `let`
    // (annotation included) and defines it in the jumping block.
    let owner = jump_owner(&cfg);
    assert!(owner.defs.contains(&s));
    assert!(stmt_text(owner).contains("let x : u32 = r ;"));
}

#[test]
fn opaque_valued_break_out_of_tail_loop_completes() {
    let block: syn::Block = parse_quote!({
        loop {
            let r = yield_!(1);
            if r > 3 {
                break r;
            }
        }
    });
    let cfg = lower_ok(&block);
    assert_eq!(cfg.opaque_jumps.len(), 1);
    assert!(matches!(cfg.opaque_jumps[0].kind, OpaqueJumpKind::Complete));
    // The marker carries the completion value.
    assert!(stmt_text(jump_owner(&cfg)).contains("__baregen_jump ! (0 , r)"));
}

#[test]
fn opaque_valued_break_into_statement_loop_is_rejected() {
    let block: syn::Block = parse_quote!({
        loop {
            yield_!(1);
            if c {
                break 5;
            }
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
fn opaque_jump_resolution_errors_match_native_ones() {
    let block: syn::Block = parse_quote!({
        yield_!(1);
        if c {
            break;
        }
        after();
    });
    assert!(error_of(&block).to_string().contains("outside of a loop"));

    let block: syn::Block = parse_quote!({
        loop {
            yield_!(1);
            if c {
                break 'nowhere;
            }
        }
    });
    assert!(error_of(&block).to_string().contains("undeclared label"));

    let block: syn::Block = parse_quote!({
        'b: {
            yield_!(1);
            if c {
                continue 'b;
            }
            after();
        }
    });
    assert!(error_of(&block).to_string().contains("labeled block"));
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
    let cfg = lower_args(&[], &block);
    assert!(cfg.opaque_jumps.is_empty());
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
    assert!(
        error_of(&block)
            .to_string()
            .contains("initializer of `let ... else`")
    );
    let block: syn::Block = parse_quote!({
        let Some(x) = f(yield_!(1)) else {
            return;
        };
    });
    assert!(
        error_of(&block)
            .to_string()
            .contains("initializer of `let ... else`")
    );
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

#[test]
fn yield_in_unsafe_block_in_value_position_is_rejected() {
    // The let-initializer form routes through `lower_value_expr`,
    // whose `Unsafe` arm is distinct from the statement-position one.
    let block: syn::Block = parse_quote!({
        let x: u32 = unsafe {
            yield_!(1);
            1
        };
    });
    assert!(error_of(&block).to_string().contains("unsafe"));
    // Same arm via the function's trailing expression.
    let block: syn::Block = parse_quote!({
        unsafe {
            yield_!(1);
            1
        }
    });
    assert!(error_of(&block).to_string().contains("unsafe"));
}

#[test]
fn unhoistable_arm_tail_in_store_context_is_rejected() {
    // The arm's trailing expression contains a yield but is not a
    // control-flow expression, so it cannot produce the `let` value.
    let block: syn::Block = parse_quote!({
        let x: u32 = if c { f(yield_!(1)) } else { 2 };
    });
    let msg = error_of(&block).to_string();
    assert!(msg.contains("value position"), "got: {msg}");
}

#[test]
fn foreign_macro_with_yield_in_tail_position_is_rejected() {
    // The tail does not count as containing a yield (foreign macro
    // tokens are opaque), so it is caught by the tail's no-yield scan.
    let block: syn::Block = parse_quote!({ println!("{}", yield_!(1)) });
    assert!(error_of(&block).to_string().contains("another macro"));
}

#[test]
fn brace_form_trailing_yield_is_rejected() {
    // A brace-delimited `yield_!` parses as `Stmt::Macro` without a
    // semicolon, taking a different path than the paren form.
    let block: syn::Block = parse_quote!({
        yield_! { 1 }
    });
    assert!(error_of(&block).to_string().contains("add a semicolon"));
}

#[test]
fn continue_outside_of_a_loop_is_rejected() {
    let block: syn::Block = parse_quote!({
        yield_!(1);
        continue;
    });
    assert!(
        error_of(&block)
            .to_string()
            .contains("`continue` outside of a loop")
    );
}

#[test]
fn continue_to_undeclared_label_is_rejected() {
    let block: syn::Block = parse_quote!({
        loop {
            yield_!(1);
            continue 'nowhere;
        }
    });
    assert!(error_of(&block).to_string().contains("undeclared label"));
}

#[test]
fn value_let_subpattern_binding_is_rejected() {
    let block: syn::Block = parse_quote!({
        let x @ _ = if c {
            yield_!(1);
            1
        } else {
            2
        };
    });
    assert!(error_of(&block).to_string().contains("simple identifier"));
}

#[test]
fn ref_resume_binding_is_rejected() {
    let block: syn::Block = parse_quote!({
        let ref r = yield_!(1);
    });
    assert!(error_of(&block).to_string().contains("simple identifier"));
}

#[test]
fn break_value_containing_yield_is_rejected() {
    // `break yield_!(..)` targeting a let-initializer loop: the value
    // is checked when it is stored to the destination binding.
    let block: syn::Block = parse_quote!({
        let x: u32 = loop {
            yield_!(0);
            break yield_!(1);
        };
        f(x);
    });
    let msg = error_of(&block).to_string();
    assert!(msg.contains("value position"), "got: {msg}");
    // The same for a trailing-expression loop, whose `break` value
    // completes the coroutine.
    let block: syn::Block = parse_quote!({
        loop {
            yield_!(0);
            break yield_!(1);
        }
    });
    let msg = error_of(&block).to_string();
    assert!(msg.contains("value position"), "got: {msg}");
}

#[test]
fn yield_in_if_let_scrutinee_is_rejected() {
    let block: syn::Block = parse_quote!({
        if let Some(x) = yield_!(1) {
            f(x);
        }
    });
    assert!(error_of(&block).to_string().contains("scrutinee"));
}

// === yield_all! delegation ===

#[test]
fn yield_all_desugars_into_a_delegation_loop() {
    let block: syn::Block = parse_quote!({
        let g: G = mk();
        yield_all!(g);
        done();
    });
    let cfg = lower_ok(&block);
    // The coroutine moves into a synthetic Delegate binding whose
    // type follows the operand variable.
    let dg = binding(&cfg, "__dg0");
    assert_eq!(cfg.bindings[dg.0].kind, BindingKind::Delegate);
    assert!(matches!(cfg.bindings[dg.0].ty, TySource::Moved(id) if id == binding(&cfg, "g")));
    // The expansion yields twice (peeled start + loop) and both
    // yields resume into the same `__rv0` binding (rebind form).
    let rv = binding(&cfg, "__rv0");
    let resume_targets: Vec<BindingId> = cfg
        .blocks
        .iter()
        .filter_map(|b| match &b.terminator {
            Terminator::Yield {
                resume_binding: Some(rb),
                ..
            } => Some(rb.binding),
            _ => None,
        })
        .collect();
    assert_eq!(resume_targets, [rv, rv]);
    // The loop's rebind resume point re-defines __rv0.
    let rebind_resume = cfg.blocks.iter().filter(|b| b.resume_point).nth(1).unwrap();
    assert!(rebind_resume.defs.contains(&rv));
}

#[test]
fn yield_all_synthetic_names_are_numbered_per_expansion() {
    let block: syn::Block = parse_quote!({
        let g1: G = mk();
        yield_all!(g1);
        let g2: G = mk();
        yield_all!(g2);
    });
    let cfg = lower_ok(&block);
    binding(&cfg, "__dg0");
    binding(&cfg, "__dg1");
    binding(&cfg, "__rv0");
    binding(&cfg, "__rv1");
}

#[test]
fn yield_all_direct_expression_is_rejected() {
    let block: syn::Block = parse_quote!({
        yield_all!(mk());
    });
    let msg = error_of(&block).to_string();
    assert!(msg.contains("single variable"), "got: {msg}");
    let block: syn::Block = parse_quote!({
        yield_all!();
    });
    assert!(error_of(&block).to_string().contains("single variable"));
}

#[test]
fn yield_all_let_destructuring_binding_is_rejected() {
    let block: syn::Block = parse_quote!({
        let (a, b) = yield_all!(g);
    });
    assert!(error_of(&block).to_string().contains("simple identifier"));
}

#[test]
fn yield_all_in_expression_position_is_rejected() {
    let block: syn::Block = parse_quote!({
        f(yield_all!(g));
    });
    assert!(error_of(&block).to_string().contains("statement position"));
}

#[test]
fn yield_all_in_condition_is_rejected() {
    let block: syn::Block = parse_quote!({
        if yield_all!(g) {
            f();
        }
    });
    let msg = error_of(&block).to_string();
    assert!(msg.contains("yield_all! is only supported"), "got: {msg}");
}

#[test]
fn yield_all_in_foreign_macro_is_rejected() {
    let block: syn::Block = parse_quote!({
        println!("{}", yield_all!(g));
    });
    assert!(error_of(&block).to_string().contains("another macro"));
}

#[test]
fn yield_all_in_let_else_initializer_is_rejected() {
    let block: syn::Block = parse_quote!({
        let x = yield_all!(g) else {
            return;
        };
    });
    assert!(
        error_of(&block)
            .to_string()
            .contains("initializer of `let ... else`")
    );
}

#[test]
fn user_written_rebind_yield_is_still_rejected() {
    // The internal `x = yield_!(..)` form must not leak into the
    // user-facing syntax.
    let block: syn::Block = parse_quote!({
        let mut x: u32 = 0;
        x = yield_!(1);
    });
    assert!(error_of(&block).to_string().contains("path, a literal"));
}

// === Argument patterns ===

#[test]
fn arg_patterns_destructure_at_entry() {
    let block: syn::Block = parse_quote!({
        sink(a + b);
    });
    let source = syn::Ident::new("__arg0", proc_macro2::Span::call_site());
    let pat: syn::Pat = parse_quote!((a, mut b));
    let cfg = lower(&[source.clone()], &[(pat, source)], &block).unwrap();
    let expected: syn::Stmt = parse_quote!(let (a, mut b) = __arg0;);
    assert_eq!(cfg.blocks[cfg.entry].stmts[0], expected);
    let a = binding(&cfg, "a");
    let b = binding(&cfg, "b");
    assert_eq!(cfg.bindings[a.0].kind, BindingKind::ArgPat);
    assert!(cfg.bindings[a.0].mutability.is_none());
    assert_eq!(cfg.bindings[b.0].kind, BindingKind::ArgPat);
    assert!(cfg.bindings[b.0].mutability.is_some());
    // The synthesized let consumes the argument.
    assert!(
        cfg.blocks[cfg.entry]
            .uses
            .contains(&binding(&cfg, "__arg0"))
    );
}

#[test]
fn duplicate_arg_pattern_binding_is_rejected() {
    let block: syn::Block = parse_quote!({});
    let a = syn::Ident::new("a", proc_macro2::Span::call_site());
    let source = syn::Ident::new("__arg1", proc_macro2::Span::call_site());
    let pat: syn::Pat = parse_quote!((a, b));
    let err = lower(&[a, source.clone()], &[(pat, source)], &block).unwrap_err();
    assert!(err.to_string().contains("bound more than once"));
}
