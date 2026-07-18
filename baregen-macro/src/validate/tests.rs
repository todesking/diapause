use std::collections::BTreeSet;

use syn::parse_quote;

use super::validate;
use crate::analyze_cfg::{Analysis, ArgInfo, analyze};
use crate::cfg::{Block, BlockId, Cfg, Terminator};
use crate::test_util::lower_args;

fn analyzed(args: &[(&str, &str)], resume_ty: &syn::Type, block: &syn::Block) -> (Cfg, Analysis) {
    let names: Vec<&str> = args.iter().map(|(n, _)| *n).collect();
    let cfg = lower_args(&names, block);
    let infos: Vec<ArgInfo> = args
        .iter()
        .map(|(_, t)| ArgInfo {
            mutability: None,
            ty: syn::parse_str(t).unwrap(),
        })
        .collect();
    let analysis = analyze(&cfg, &infos, resume_ty).unwrap();
    (cfg, analysis)
}

fn assert_valid(args: &[(&str, &str)], block: &syn::Block) {
    let unit: syn::Type = parse_quote!(());
    let (cfg, analysis) = analyzed(args, &unit, block);
    if let Err(msg) = validate(&cfg, &analysis, args.len()) {
        panic!("expected valid IR for {block:?}, got:\n{msg}");
    }
}

/// Resume-point blocks, in yield order.
fn resume_ids(cfg: &Cfg) -> Vec<BlockId> {
    (0..cfg.blocks.len())
        .filter(|b| cfg.blocks[*b].resume_point)
        .collect()
}

// === The pass accepts real lowered coroutines ===

#[test]
fn representative_bodies_validate() {
    let bodies: Vec<syn::Block> = vec![
        parse_quote!({
            let a: u32 = 1;
            yield_!(a);
            let r = yield_!(2);
            r
        }),
        parse_quote!({
            let mut sum: u32 = 0;
            let mut i: u32 = 0;
            while i < n {
                let r = yield_!(sum);
                sum += r;
                i += 1;
            }
            sum
        }),
        parse_quote!({
            let mut sum: u32 = 0;
            for i in 0u32..n {
                yield_!(sum);
                sum += i;
            }
            sum
        }),
        parse_quote!({
            let x: u32 = if c {
                yield_!(1);
                1
            } else {
                2
            };
            yield_!(x);
            f(x)
        }),
        parse_quote!({
            match n {
                0 => {}
                _ => {
                    yield_!(1);
                }
            }
            g()
        }),
        // Opaque jump: the yield-free `if` stays a statement and its
        // `break` becomes a `__baregen_jump!` marker.
        parse_quote!({
            let mut i: u32 = 0;
            loop {
                yield_!(i);
                if i > 3 {
                    break;
                }
                i = i + 1;
            }
            f(i)
        }),
        // Borrow reconstruction across a yield.
        parse_quote!({
            let mut x: u32 = 0;
            let y = &mut x;
            yield_!(1);
            *y += 1;
            x
        }),
        parse_quote!({
            let Some(p) = o else {
                yield_!(0);
                return;
            };
            let p2: u32 = p;
            yield_!(p2);
        }),
    ];
    for body in &bodies {
        assert_valid(&[("n", "u32"), ("c", "bool"), ("o", "Option<u32>")], body);
    }
}

#[test]
fn yield_all_delegation_validates() {
    let block: syn::Block = parse_quote!({
        let g: G = mk();
        let x: u32 = yield_all!(g);
        f(x);
    });
    let resume_ty: syn::Type = parse_quote!(u32);
    let (cfg, analysis) = analyzed(&[], &resume_ty, &block);
    validate(&cfg, &analysis, 0).unwrap();
}

// === Broken CFG structure is reported ===

fn simple() -> (Cfg, Analysis) {
    let unit: syn::Type = parse_quote!(());
    analyzed(
        &[],
        &unit,
        &parse_quote!({
            let a: u32 = 1;
            yield_!(0);
            f(a);
        }),
    )
}

fn err_of(cfg: &Cfg, analysis: &Analysis, n_args: usize) -> String {
    validate(cfg, analysis, n_args).expect_err("expected a validation failure")
}

#[test]
fn dangling_terminator_target_is_reported() {
    let (mut cfg, analysis) = simple();
    let last = cfg.blocks.len() - 1;
    cfg.blocks[last].terminator = Terminator::Goto(999);
    let msg = err_of(&cfg, &analysis, 0);
    assert!(msg.contains("nonexistent block 999"), "got: {msg}");
}

#[test]
fn unreachable_block_is_reported() {
    let (mut cfg, mut analysis) = simple();
    cfg.blocks.push(Block {
        stmts: Vec::new(),
        terminator: Terminator::Return(parse_quote!(())),
        uses: BTreeSet::new(),
        defs: BTreeSet::new(),
        resume_point: false,
        jumps: Vec::new(),
        inline: false,
    });
    analysis.live_in.push(BTreeSet::new());
    analysis.removed_stmts.push(BTreeSet::new());
    let msg = err_of(&cfg, &analysis, 0);
    assert!(msg.contains("unreachable"), "got: {msg}");
    assert!(msg.contains("no state variant"), "got: {msg}");
}

#[test]
fn inline_resume_point_is_reported() {
    let (mut cfg, analysis) = simple();
    let s1 = resume_ids(&cfg)[0];
    cfg.blocks[s1].inline = true;
    let msg = err_of(&cfg, &analysis, 0);
    assert!(msg.contains("marked inline"), "got: {msg}");
}

#[test]
fn non_resume_yield_continuation_is_reported() {
    let (mut cfg, analysis) = simple();
    let s1 = resume_ids(&cfg)[0];
    cfg.blocks[s1].resume_point = false;
    let msg = err_of(&cfg, &analysis, 0);
    assert!(msg.contains("not a resume point"), "got: {msg}");
}

#[test]
fn missing_dispatch_variant_is_reported() {
    let (cfg, mut analysis) = simple();
    let s1 = resume_ids(&cfg)[0];
    analysis.variants.retain(|v| v.block != s1);
    let msg = err_of(&cfg, &analysis, 0);
    assert!(msg.contains("no state variant"), "got: {msg}");
    assert!(msg.contains("dispatch"), "got: {msg}");
}

#[test]
fn jump_marker_desync_is_reported() {
    let unit: syn::Type = parse_quote!(());
    let (mut cfg, analysis) = analyzed(
        &[],
        &unit,
        &parse_quote!({
            let mut i: u32 = 0;
            loop {
                yield_!(i);
                if i > 3 {
                    break;
                }
                i = i + 1;
            }
            f(i)
        }),
    );
    let owner = (0..cfg.blocks.len())
        .find(|b| !cfg.blocks[*b].jumps.is_empty())
        .expect("expected a block owning an opaque jump");
    cfg.blocks[owner].jumps.clear();
    let msg = err_of(&cfg, &analysis, 0);
    assert!(msg.contains("jump markers"), "got: {msg}");
}
