//! Snapshot tests for the DOT export: representative coroutine bodies
//! are run through the full debug pipeline and the rendered graph is
//! compared verbatim.

use quote::quote;
use syn::parse_quote;

use baregen_macro_core::expand_debug;

use super::cfg_to_dot;

/// Runs the debug pipeline and renders (unsimplified without analysis,
/// simplified with analysis).
fn dots(attr: proc_macro2::TokenStream, item: syn::ItemFn) -> (String, String) {
    let dbg = expand_debug(attr, item);
    dbg.result.expect("expansion should succeed");
    let pre = cfg_to_dot(&dbg.cfg_unsimplified.expect("unsimplified CFG"), None);
    let post = cfg_to_dot(
        &dbg.cfg.expect("simplified CFG"),
        Some(&dbg.analysis.expect("analysis")),
    );
    (pre, post)
}

#[test]
fn if_body() {
    let (pre, post) = dots(
        quote!(yield = u32),
        parse_quote! {
            fn c(n: u32) -> u32 {
                if n > 0 {
                    yield_!(n);
                }
                n
            }
        },
    );
    assert_eq!(
        pre,
        r#"digraph cfg {
    rankdir=TB;
    node [shape=box, fontname="Courier"];
    edge [fontname="Courier"];
    b0 [label="b0 (entry)\lif n > 0\l"];
    b1 [label="b1\lreturn n\l"];
    b2 [label="b2\lyield n\l"];
    b3 [label="b3 (resume)\l", peripheries=2];
    b0 -> b2 [label="then"];
    b0 -> b1 [label="else"];
    b2 -> b3 [label="resume"];
    b3 -> b1;
}
"#
    );
    assert_eq!(
        post,
        r#"digraph cfg {
    rankdir=TB;
    node [shape=box, fontname="Courier"];
    edge [fontname="Courier"];
    b0 [label="b0 [Start] (entry)\lif n > 0\l"];
    b1 [label="b1 [B1]\lreturn n\l"];
    b2 [label="b2 (inline)\lyield n\l", style=dashed];
    b3 [label="b3 [S1] (resume)\l", peripheries=2];
    b0 -> b2 [label="then"];
    b0 -> b1 [label="else"];
    b2 -> b3 [label="resume"];
    b3 -> b1;
}
"#
    );
}

#[test]
fn match_body() {
    let (pre, post) = dots(
        quote!(yield = u32),
        parse_quote! {
            fn c(n: u32) {
                match n {
                    0 => yield_!(0u32),
                    k if k > 10 => yield_!(k),
                    _ => {}
                }
            }
        },
    );
    assert_eq!(
        pre,
        r#"digraph cfg {
    rankdir=TB;
    node [shape=box, fontname="Courier"];
    edge [fontname="Courier"];
    b0 [label="b0 (entry)\lmatch n\l"];
    b1 [label="b1\lreturn ()\l"];
    b2 [label="b2\lyield 0u32\l"];
    b3 [label="b3 (resume)\lreturn __tmp0\l", peripheries=2];
    b4 [label="b4\lyield k\l"];
    b5 [label="b5 (resume)\lreturn __tmp1\l", peripheries=2];
    b6 [label="b6\lreturn {}\l"];
    b0 -> b2 [label="0"];
    b0 -> b4 [label="k if k > 10"];
    b0 -> b6 [label="_"];
    b2 -> b3 [label="resume __tmp0"];
    b4 -> b5 [label="resume __tmp1"];
}
"#
    );
    assert_eq!(
        post,
        r#"digraph cfg {
    rankdir=TB;
    node [shape=box, fontname="Courier"];
    edge [fontname="Courier"];
    b0 [label="b0 [Start] (entry)\lmatch n\l"];
    b1 [label="b1 (inline)\lyield 0u32\l", style=dashed];
    b2 [label="b2 [S1] (resume)\lreturn __tmp0\l", peripheries=2];
    b3 [label="b3 (inline)\lyield k\l", style=dashed];
    b4 [label="b4 [S2] (resume)\lreturn __tmp1\l", peripheries=2];
    b5 [label="b5 (inline)\lreturn {}\l", style=dashed];
    b0 -> b1 [label="0"];
    b0 -> b3 [label="k if k > 10"];
    b0 -> b5 [label="_"];
    b1 -> b2 [label="resume __tmp0"];
    b3 -> b4 [label="resume __tmp1"];
}
"#
    );
}

#[test]
fn loop_body_with_opaque_break() {
    // `if i > 3 { break; }` contains no yield, so it stays an opaque
    // statement and the `break` becomes an opaque-jump edge (dashed).
    let (pre, post) = dots(
        quote!(yield = u32),
        parse_quote! {
            fn c() -> u32 {
                let mut i = 0u32;
                loop {
                    yield_!(i);
                    i += 1;
                    if i > 3 {
                        break;
                    }
                }
                i
            }
        },
    );
    assert_eq!(
        pre,
        r#"digraph cfg {
    rankdir=TB;
    node [shape=box, fontname="Courier"];
    edge [fontname="Courier"];
    b0 [label="b0 (entry)\llet mut i = 0u32;\l"];
    b1 [label="b1\lyield i\l"];
    b2 [label="b2\lreturn i\l"];
    b3 [label="b3 (resume)\li += 1;\lif i > 3 { __baregen_jump!(0); }\l", peripheries=2];
    b0 -> b1;
    b1 -> b3 [label="resume"];
    b3 -> b1;
    b3 -> b2 [label="jump", style=dashed];
}
"#
    );
    assert_eq!(
        post,
        r#"digraph cfg {
    rankdir=TB;
    node [shape=box, fontname="Courier"];
    edge [fontname="Courier"];
    b0 [label="b0 [Start] (entry)\llet mut i = 0u32;\l"];
    b1 [label="b1 [B1]\lyield i\l"];
    b2 [label="b2 [B2]\lreturn i\l"];
    b3 [label="b3 [S1] (resume)\li += 1;\lif i > 3 { __baregen_jump!(0); }\l", peripheries=2];
    b0 -> b1;
    b1 -> b3 [label="resume"];
    b3 -> b1;
    b3 -> b2 [label="jump", style=dashed];
}
"#
    );
}

#[test]
fn tail_loop_with_completing_break() {
    // A valued `break` in the function's tail loop completes the
    // coroutine: the jump renders as a dashed edge into the synthetic
    // `complete` node.
    let (_, post) = dots(
        quote!(yield = u32),
        parse_quote! {
            fn c() -> u32 {
                loop {
                    yield_!(1u32);
                    if f() {
                        break 42;
                    }
                }
            }
        },
    );
    assert_eq!(
        post,
        r#"digraph cfg {
    rankdir=TB;
    node [shape=box, fontname="Courier"];
    edge [fontname="Courier"];
    b0 [label="b0 [Start] (entry)\l"];
    b1 [label="b1 [B1]\lyield 1u32\l"];
    b2 [label="b2 [S1] (resume)\lif f() { __baregen_jump!(0, 42); }\l", peripheries=2];
    complete [shape=ellipse, label="complete"];
    b0 -> b1;
    b1 -> b2 [label="resume"];
    b2 -> b1;
    b2 -> complete [style=dashed, label="jump"];
}
"#
    );
}

#[test]
fn for_body() {
    let (pre, post) = dots(
        quote!(yield = u32),
        parse_quote! {
            fn c(v: [u32; 3]) {
                for x in v {
                    yield_!(x);
                }
            }
        },
    );
    assert_eq!(
        pre,
        r#"digraph cfg {
    rankdir=TB;
    node [shape=box, fontname="Courier"];
    edge [fontname="Courier"];
    b0 [label="b0 (entry)\llet mut __iter0 = ::core::iter::IntoIterator::into_iter(v);\l"];
    b1 [label="b1\l__iter0.next()\l"];
    b2 [label="b2\lyield x\l"];
    b3 [label="b3\lreturn ()\l"];
    b4 [label="b4 (resume)\l", peripheries=2];
    b0 -> b1;
    b1 -> b2 [label="Some(x)"];
    b1 -> b3 [label="None"];
    b2 -> b4 [label="resume"];
    b4 -> b1;
}
"#
    );
    assert_eq!(
        post,
        r#"digraph cfg {
    rankdir=TB;
    node [shape=box, fontname="Courier"];
    edge [fontname="Courier"];
    b0 [label="b0 [Start] (entry)\llet mut __iter0 = ::core::iter::IntoIterator::into_iter(v);\l"];
    b1 [label="b1 [B1]\l__iter0.next()\l"];
    b2 [label="b2 (inline)\lyield x\l", style=dashed];
    b3 [label="b3 (inline)\lreturn ()\l", style=dashed];
    b4 [label="b4 [S1] (resume)\l", peripheries=2];
    b0 -> b1;
    b1 -> b2 [label="Some(x)"];
    b1 -> b3 [label="None"];
    b2 -> b4 [label="resume"];
    b4 -> b1;
}
"#
    );
}

#[test]
fn yield_all_body() {
    let (pre, post) = dots(
        quote!(yield = u32),
        parse_quote! {
            fn c(sub: Sub) {
                yield_!(0u32);
                yield_all!(sub);
            }
        },
    );
    assert_eq!(
        pre,
        r#"digraph cfg {
    rankdir=TB;
    node [shape=box, fontname="Courier"];
    edge [fontname="Courier"];
    b0 [label="b0 (entry)\lyield 0u32\l"];
    b1 [label="b1 (resume)\llet mut __dg0 = sub;\lmatch ::baregen::Coroutine::start(&mut __dg0)\l", peripheries=2];
    b2 [label="b2\lreturn ()\l"];
    b3 [label="b3\l{}\l"];
    b4 [label="b4\lyield __y0\l"];
    b5 [label="b5 (resume)\l", peripheries=2];
    b6 [label="b6\lmatch ::baregen::Coroutine::resume(&mut __dg0, __rv0)\l"];
    b7 [label="b7\l"];
    b8 [label="b8\l"];
    b9 [label="b9\l"];
    b10 [label="b10\l"];
    b11 [label="b11\lyield __y0\l"];
    b12 [label="b12 (resume)\l", peripheries=2];
    b0 -> b1 [label="resume"];
    b1 -> b3 [label="::baregen::CoroutineState::Complete(_)"];
    b1 -> b4 [label="::baregen::CoroutineState::Yielded(__y0)"];
    b3 -> b2;
    b4 -> b5 [label="resume __rv0"];
    b5 -> b6;
    b6 -> b9 [label="::baregen::CoroutineState::Complete(_)"];
    b6 -> b11 [label="::baregen::CoroutineState::Yielded(__y0)"];
    b7 -> b2;
    b8 -> b6;
    b9 -> b7;
    b10 -> b8;
    b11 -> b12 [label="resume __rv0"];
    b12 -> b8;
}
"#
    );
    assert_eq!(
        post,
        r#"digraph cfg {
    rankdir=TB;
    node [shape=box, fontname="Courier"];
    edge [fontname="Courier"];
    b0 [label="b0 [Start] (entry)\lyield 0u32\l"];
    b1 [label="b1 [S1] (resume)\llet mut __dg0 = sub;\lmatch ::baregen::Coroutine::start(&mut __dg0)\l", peripheries=2];
    b2 [label="b2 [B2]\lreturn ()\l"];
    b3 [label="b3 (inline)\l{}\l", style=dashed];
    b4 [label="b4 (inline)\lyield __y0\l", style=dashed];
    b5 [label="b5 [S2] (resume)\l", peripheries=2];
    b6 [label="b6 [B1]\lmatch ::baregen::Coroutine::resume(&mut __dg0, __rv0)\l"];
    b7 [label="b7 (inline)\l", style=dashed];
    b8 [label="b8 (inline)\lyield __y0\l", style=dashed];
    b9 [label="b9 [S3] (resume)\l", peripheries=2];
    b0 -> b1 [label="resume"];
    b1 -> b3 [label="::baregen::CoroutineState::Complete(_)"];
    b1 -> b4 [label="::baregen::CoroutineState::Yielded(__y0)"];
    b3 -> b2;
    b4 -> b5 [label="resume __rv0"];
    b5 -> b6;
    b6 -> b7 [label="::baregen::CoroutineState::Complete(_)"];
    b6 -> b8 [label="::baregen::CoroutineState::Yielded(__y0)"];
    b7 -> b2;
    b8 -> b9 [label="resume __rv0"];
    b9 -> b6;
}
"#
    );
}

#[test]
fn escapes_quotes_in_labels() {
    let (_, post) = dots(
        quote!(yield = u32),
        parse_quote! {
            fn c() {
                let s = "a \"quoted\" str";
                let _ = s;
                yield_!(0u32);
            }
        },
    );
    assert!(post.contains(r#"let s = \"a \\\"quoted\\\" str\";"#));
}
