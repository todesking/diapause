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

fn run_args(args: &[(&str, &str)], block: &syn::Block, resume_ty: &syn::Type) -> (Cfg, Analysis) {
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
    &a.variant(block)
        .expect("expected a variant block")
        .reborrows
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
fn inclusive_for_iterator_resolves_to_the_wrapper_type() {
    let block: syn::Block = parse_quote!({
        let r = 0u32..=n;
        for i in r {
            yield_!(i);
        }
        0u32
    });
    let (cfg, a) = run_args(&[("n", "u32")], &block, &unit());
    let s1 = resume_ids(&cfg)[0];
    let wrapper: syn::Type = parse_quote!(__RangeInclusiveIter<u32>);
    assert_eq!(field(&a, s1, "__iter0").ty, wrapper);
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
    let iter_ty: syn::Type =
        parse_quote!(<::core::ops::Range<u32> as ::core::iter::IntoIterator>::IntoIter);
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
    let iter_ty: syn::Type = parse_quote!(<[u32; 3] as ::core::iter::IntoIterator>::IntoIter);
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
fn opaque_jump_shadowing_a_state_field_is_rejected() {
    // The `if let` statement contains no yield, so it stays opaque and
    // its `break` becomes an opaque jump into the loop's exit variant,
    // which stores the outer `sum`; the arm binding `sum` inside the
    // statement would be captured instead of the stored one.
    let block: syn::Block = parse_quote!({
        let mut sum: u32 = 0;
        loop {
            yield_!(sum);
            if let Some(sum) = helper(n) {
                if sum > 3 {
                    break;
                }
            }
            sum += 1;
        }
        f(sum);
    });
    let (_, result) = lower_analyze(&[("n", "u32")], &block, &unit());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("this `sum` shadows a variable"), "got: {msg}");
    assert!(msg.contains("rename the inner binding"), "got: {msg}");
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
    assert!(
        msg.contains("two different bindings named `x`"),
        "got: {msg}"
    );
}

// === Transfer shadowing ===

#[test]
fn borrow_source_shadowed_after_yield_is_rejected() {
    // The original silent-miscompile repro (2026-07-17): the shadowing
    // `x` is dead at every variant entry, but it is in scope at S1's
    // transition, which moves the borrow source `x` by name into S2.
    let block: syn::Block = parse_quote!({
        let x: u32 = 1;
        let y = &x;
        yield_!(0);
        let x: u32 = 99;
        let _ = x;
        yield_!(0);
        f(*y);
    });
    let msg = error_of(&block).to_string();
    assert!(msg.contains("shadows an earlier binding `x`"), "got: {msg}");
    assert!(msg.contains("rename one of them"), "got: {msg}");
}

#[test]
fn borrow_source_shadowed_before_yield_is_rejected() {
    // Within-chain variant: source and shadow sit in the same block, so
    // the transition into S1 already captures the shadowing `x` (the
    // borrow `let` is dropped and `y` rebuilt from the stored `x`).
    // Confirmed as a silent wrong-value miscompile by a runtime probe
    // on 2026-07-18.
    let block: syn::Block = parse_quote!({
        let x: u32 = 1;
        let y = &x;
        let x: u32 = 99;
        let _ = x;
        yield_!(0);
        f(*y);
    });
    let msg = error_of(&block).to_string();
    assert!(msg.contains("shadows an earlier binding `x`"), "got: {msg}");
}

#[test]
fn shadowing_before_an_opaque_jump_is_rejected() {
    // The shadowing `let` precedes the statement carrying the `break`,
    // so it is in scope at the jump, which moves the borrow source `x`
    // into the loop's exit state.
    let block: syn::Block = parse_quote!({
        let x: u32 = 1;
        let y = &x;
        yield_!(0);
        loop {
            let x: u32 = 99;
            let _ = x;
            if c {
                break;
            }
            yield_!(0);
        }
        f(*y);
    });
    let (_, result) = lower_analyze(&[("c", "bool")], &block, &unit());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("shadows an earlier binding `x`"), "got: {msg}");
}

#[test]
fn sequential_shadowing_after_borrow_dies_is_fine() {
    // The borrow's last use precedes the shadowing `let`, so only the
    // new `x` crosses the second yield and the transfer resolves to it.
    let block: syn::Block = parse_quote!({
        let x: u32 = 1;
        let y = &x;
        yield_!(0);
        f(*y);
        let x: u32 = 99;
        yield_!(0);
        f(x);
    });
    let (_, result) = lower_analyze(&[], &block, &unit());
    assert!(result.is_ok(), "got: {:?}", result.err());
}

#[test]
fn shadowing_inside_an_opaque_scope_is_fine() {
    // The braced block stays an opaque statement, so its inner `x` is
    // scoped by the emitted braces and shadows nothing at the transfer.
    let block: syn::Block = parse_quote!({
        let x: u32 = 1;
        let y = &x;
        yield_!(0);
        {
            let x: u32 = 99;
            let _ = x;
        }
        yield_!(0);
        f(*y);
    });
    let (_, result) = lower_analyze(&[], &block, &unit());
    assert!(result.is_ok(), "got: {:?}", result.err());
}

#[test]
fn removed_borrow_shadow_is_fine() {
    // The shadowing binding is itself a borrow whose `let` is removed
    // from the emitted arm (rebuilt in the next region), so the outer
    // `y` is transferred untouched.
    let block: syn::Block = parse_quote!({
        let y: u32 = 1;
        let p = &y;
        yield_!(0);
        let x: u32 = 2;
        let y = &x;
        yield_!(0);
        f(*p + *y);
    });
    let (_, result) = lower_analyze(&[], &block, &unit());
    assert!(result.is_ok(), "got: {:?}", result.err());
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
    let cfg = crate::lower::lower(
        std::slice::from_ref(&source),
        &[(pat, source.clone())],
        block,
    )
    .unwrap();
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
