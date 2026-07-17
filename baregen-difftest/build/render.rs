//! Renders a generated case into a test-case module. The body text is
//! rendered once per world, but every construct except `Delegate`
//! produces identical text in both worlds (the renderer branches on
//! the world only in the `Delegate` arm), so the coroutine and the
//! reference cannot diverge textually anywhere else.

use crate::ast::*;

/// Which function the body is being rendered into. `Delegate` is the
/// only construct that renders differently: the reference world calls
/// the sub-case's reference function directly, because the
/// sub-coroutine's state type does not exist there.
#[derive(Clone, Copy, PartialEq)]
enum World {
    Coroutine,
    Reference,
}

/// Everything render functions need besides the AST node: the target
/// world and whether yields render argument-less (`yield = ()`).
#[derive(Clone, Copy)]
struct Ctx {
    world: World,
    unit_yield: bool,
}

pub fn render_case(idx: usize, case: &Case) -> String {
    let name = format!("case_{idx:03}");
    let ret = match case.flavor {
        Flavor::U32 => " -> u32",
        Flavor::OptionU32 => " -> Option<u32>",
        // Fully qualified: the coroutine attribute moves the body into a
        // generated module, so bare imported names would not resolve.
        Flavor::ResultU32 => " -> Result<u32, baregen_difftest::Err1>",
        // The unit return type is either spelled out or elided entirely.
        Flavor::Unit => {
            if case.explicit_unit {
                " -> ()"
            } else {
                ""
            }
        }
    };
    let yield_ty = if case.unit_yield { "()" } else { "u32" };
    let fp = match case.fingerprint {
        FpMode::Off => "",
        FpMode::Source => ", fingerprint",
        // The manual-tag string form; the tag's content is irrelevant
        // to a single-binary run, it only has to parse and hash.
        FpMode::Tag => ", fingerprint = \"difftest-tag\"",
    };
    let attr = format!("#[baregen::coroutine(yield = {yield_ty}, resume = u32{fp})]");
    let sig = match case.shape {
        ArgShape::Plain => "a0: u32, a1: u32",
        ArgShape::Tuple => "(b0, b1): (u32, u32)",
        ArgShape::Mixed => "(b0, b1): (u32, u32), a2: u32",
    };
    // Pattern bindings have no type annotation, so they must not cross
    // a yield; the body rebinds them to the annotated pool names first.
    let rebind = |level: usize| match case.shape {
        ArgShape::Plain => String::new(),
        _ => format!(
            "{i}let a0: u32 = b0;\n{i}let a1: u32 = b1;\n",
            i = ind(level)
        ),
    };
    // Proptest draws one u32 per argument value; the call re-groups
    // them to match the signature's shape.
    let (extra_param, args_list, call) = match case.shape {
        ArgShape::Plain => ("", "a0, a1", "a0, a1"),
        ArgShape::Tuple => ("", "a0, a1", "(a0, a1)"),
        ArgShape::Mixed => ("\n        a2 in 0u32..16u32,", "a0, a1, a2", "(a0, a1), a2"),
    };
    let co_ctx = Ctx {
        world: World::Coroutine,
        unit_yield: case.unit_yield,
    };
    let ref_ctx = Ctx {
        world: World::Reference,
        unit_yield: case.unit_yield,
    };
    let co_body = format!("{}{}", rebind(2), render_body(&case.body, 2, co_ctx));
    let ref_body = format!("{}{}", rebind(2), render_body(&case.body, 2, ref_ctx));
    let source = format!(
        "{attr}\nfn co({sig}){ret} {{\n{}{}}}",
        rebind(1),
        render_body(&case.body, 1, co_ctx)
    );
    format!(
        r####"
#[allow(
    unused_variables,
    unused_mut,
    unused_assignments,
    unused_imports,
    unused_labels,
    unused_parens,
    unused_comparisons,
    unused_must_use,
    unreachable_code,
    dead_code,
    clippy::all
)]
mod {name} {{
    use baregen_difftest::yield_;

    pub const SOURCE: &str = r##"{source}"##;

    {attr}
    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    pub fn co({sig}){ret} {{
{co_body}    }}

    pub fn reference({sig}){ret} {{
{ref_body}    }}
}}

proptest::proptest! {{
    #![proptest_config(baregen_difftest::proptest_config())]
    #[test]
    fn {name}(
        a0 in 0u32..16u32,
        a1 in 0u32..16u32,{extra_param}
        resumes in proptest::collection::vec(0u32..16u32, 1..12),
    ) {{
        baregen_difftest::check_case(
            {name}::SOURCE,
            &[{args_list}],
            &resumes,
            || {name}::reference({call}),
            {name}::co({call}),
        );
    }}
}}
"####
    )
}

fn ind(level: usize) -> String {
    "    ".repeat(level)
}

fn render_body(body: &Body, level: usize, ctx: Ctx) -> String {
    let mut out = String::new();
    for s in &body.stmts {
        render_stmt(&mut out, s, level, ctx);
    }
    let i = ind(level);
    match &body.tail {
        Tail::Ret(r) => out.push_str(&format!("{i}{}\n", ret_expr(r))),
        Tail::Yield(e) => out.push_str(&format!("{i}{}\n", yc(e, ctx))),
        Tail::YieldWrapped(e) => out.push_str(&format!("{i}Some({})\n", yc(e, ctx))),
        Tail::YieldOk(e) => out.push_str(&format!("{i}Ok({})\n", yc(e, ctx))),
        Tail::YieldUnit(e) => out.push_str(&format!("{i}{};\n", yc(e, ctx))),
        Tail::ImplicitUnit => {}
        Tail::BreakLoop {
            counter,
            limit,
            label,
            fuel,
            body,
        } => {
            out.push_str(&format!("{i}let mut {counter}: u32 = 0u32;\n"));
            out.push_str(&format!("{i}'l{label}: loop {{\n"));
            out.push_str(&format!(
                "{}if {counter} >= {limit}u32 {{\n{}break 'l{label} {};\n{}}}\n",
                ind(level + 1),
                ind(level + 2),
                ret_expr(fuel),
                ind(level + 1)
            ));
            out.push_str(&format!(
                "{}{counter} = {counter}.wrapping_add(1u32);\n",
                ind(level + 1)
            ));
            render_block(&mut out, body, level + 1, ctx);
            out.push_str(&format!("{i}}}\n"));
        }
        // Trailing delegation: the coroutine world delegates with
        // `yield_all!`, the reference world calls the sub-case's
        // reference function; either way the result is the tail value.
        Tail::Delegate {
            sub_case,
            sub_shape,
            sub_var,
            args,
        } => {
            let call = call_args(*sub_shape, args);
            let sub_mod = format!("crate::case_{sub_case:03}");
            match ctx.world {
                World::Coroutine => {
                    out.push_str(&format!(
                        "{i}let {sub_var}: {sub_mod}::co::State = {sub_mod}::co({call});\n"
                    ));
                    out.push_str(&format!("{i}yield_all!({sub_var})\n"));
                }
                World::Reference => {
                    out.push_str(&format!("{i}{sub_mod}::reference({call})\n"));
                }
            }
        }
    }
    out
}

fn render_block(out: &mut String, stmts: &[Stmt], level: usize, ctx: Ctx) {
    for s in stmts {
        render_stmt(out, s, level, ctx);
    }
}

fn render_stmt(out: &mut String, s: &Stmt, l: usize, ctx: Ctx) {
    let i = ind(l);
    match s {
        Stmt::Let { name, expr: e } => {
            out.push_str(&format!("{i}let mut {name}: u32 = {};\n", expr(e)));
        }
        Stmt::LetOption { name, init } => {
            let init = match init {
                Some(e) => format!("Some({})", expr(e)),
                None => "None".to_string(),
            };
            out.push_str(&format!("{i}let mut {name}: Option<u32> = {init};\n"));
        }
        Stmt::Assign { name, expr: e } => {
            out.push_str(&format!("{i}{name} = {};\n", expr(e)));
        }
        Stmt::Yield(e) => {
            out.push_str(&format!("{i}{};\n", yc(e, ctx)));
        }
        Stmt::LetYield { name, arg, annot } => {
            let ty = if *annot { ": u32" } else { "" };
            out.push_str(&format!("{i}let {name}{ty} = {};\n", yc(arg, ctx)));
        }
        Stmt::LetInfer { name, from } => {
            out.push_str(&format!("{i}let mut {name} = {from};\n"));
        }
        Stmt::LetNegLit { name, val } => {
            out.push_str(&format!("{i}let {name} = -{val}i32;\n"));
        }
        Stmt::Closure {
            name,
            add,
            block_body,
            out: o,
            arg,
        } => {
            if *block_body {
                out.push_str(&format!(
                    "{i}let {name} = |v: u32| -> u32 {{ v.wrapping_add({add}u32) }};\n"
                ));
            } else {
                out.push_str(&format!(
                    "{i}let {name} = |v: u32| v.wrapping_add({add}u32);\n"
                ));
            }
            out.push_str(&format!("{i}let mut {o}: u32 = {name}({});\n", expr(arg)));
        }
        Stmt::ForeignMacro { out: o, arg } => match o {
            Some(o) => {
                out.push_str(&format!(
                    "{i}let mut {o}: u32 = (format!(\"x{{}}\", {}).len() as u32);\n",
                    expr(arg)
                ));
            }
            None => {
                out.push_str(&format!("{i}format!(\"{{:?}}\", {});\n", expr(arg)));
            }
        },
        Stmt::NestedFn {
            name,
            mul,
            out: o,
            arg,
        } => {
            out.push_str(&format!(
                "{i}fn {name}(v: u32) -> u32 {{\n{}v.wrapping_mul({mul}u32)\n{i}}}\n",
                ind(l + 1)
            ));
            out.push_str(&format!("{i}let mut {o}: u32 = {name}({});\n", expr(arg)));
        }
        Stmt::LetYieldAdd { name, a, b } => {
            out.push_str(&format!(
                "{i}let {name}: u32 = u32::wrapping_add({}, {});\n",
                yc(a, ctx),
                yc(b, ctx)
            ));
        }
        Stmt::AssignYieldAdd { name, arg } => {
            out.push_str(&format!(
                "{i}{name} = u32::wrapping_add({name}, {});\n",
                yc(arg, ctx)
            ));
        }
        Stmt::XorAssignYield { name, arg } => {
            out.push_str(&format!("{i}{name} ^= {};\n", yc(arg, ctx)));
        }
        Stmt::LetArray { name, elems } => {
            let elems: Vec<String> = elems.iter().map(expr).collect();
            out.push_str(&format!(
                "{i}let {name}: [u32; 4] = [{}];\n",
                elems.join(", ")
            ));
        }
        Stmt::LetTry { name, opt } => {
            out.push_str(&format!("{i}let {name}: u32 = {opt}?;\n"));
        }
        Stmt::LetResult { name, init } => {
            let init = match init {
                Ok(e) => format!("Ok({})", expr(e)),
                Err(e) => format!("Err(baregen_difftest::Err2({}))", expr(e)),
            };
            out.push_str(&format!(
                "{i}let {name}: Result<u32, baregen_difftest::Err2> = {init};\n"
            ));
        }
        Stmt::LetTryResult { name, res } => {
            out.push_str(&format!("{i}let {name}: u32 = {res}?;\n"));
        }
        Stmt::If {
            cond: c,
            then_b,
            else_ifs,
            else_b,
        } => {
            out.push_str(&format!("{i}if {} {{\n", cond(c)));
            render_block(out, then_b, l + 1, ctx);
            for (c, b) in else_ifs {
                out.push_str(&format!("{i}}} else if {} {{\n", cond(c)));
                render_block(out, b, l + 1, ctx);
            }
            if let Some(eb) = else_b {
                out.push_str(&format!("{i}}} else {{\n"));
                render_block(out, eb, l + 1, ctx);
            }
            out.push_str(&format!("{i}}}\n"));
        }
        Stmt::IfLet {
            opt,
            bind,
            rebind,
            then_b,
            else_b,
        } => {
            out.push_str(&format!("{i}if let Some({bind}) = {opt} {{\n"));
            out.push_str(&format!("{}let mut {rebind}: u32 = {bind};\n", ind(l + 1)));
            render_block(out, then_b, l + 1, ctx);
            if let Some(eb) = else_b {
                out.push_str(&format!("{i}}} else {{\n"));
                render_block(out, eb, l + 1, ctx);
            }
            out.push_str(&format!("{i}}}\n"));
        }
        Stmt::Match {
            scrut,
            modulus,
            arms,
        } => {
            out.push_str(&format!("{i}match ({}) % {modulus}u32 {{\n", expr(scrut)));
            for (j, (guard, arm)) in arms.iter().enumerate() {
                let pat = arm_pat(j, *modulus, guard);
                out.push_str(&format!("{}{pat} => {{\n", ind(l + 1)));
                render_block(out, arm, l + 2, ctx);
                out.push_str(&format!("{}}}\n", ind(l + 1)));
            }
            out.push_str(&format!("{i}}}\n"));
        }
        Stmt::MatchYield {
            scrut,
            modulus,
            arms,
        } => {
            out.push_str(&format!("{i}match ({}) % {modulus}u32 {{\n", expr(scrut)));
            for (j, (guard, yields, e)) in arms.iter().enumerate() {
                let pat = arm_pat(j, *modulus, guard);
                let body = if *yields { yc(e, ctx) } else { expr(e) };
                out.push_str(&format!("{}{pat} => {body},\n", ind(l + 1)));
            }
            out.push_str(&format!("{i}}};\n"));
        }
        Stmt::LetElse {
            scrut,
            modulus,
            body,
        } => {
            out.push_str(&format!(
                "{i}let 0u32 = ({}) % {modulus}u32 else {{\n",
                expr(scrut)
            ));
            render_block(out, body, l + 1, ctx);
            out.push_str(&format!("{i}}};\n"));
        }
        Stmt::For {
            var,
            upper,
            inclusive,
            label,
            body,
        } => {
            let up = match upper {
                Upper::Lit(n) => format!("{n}u32"),
                Upper::Var(v) => v.clone(),
            };
            let dots = if *inclusive { "..=" } else { ".." };
            out.push_str(&format!("{i}'l{label}: for {var} in 0u32{dots}{up} {{\n"));
            render_block(out, body, l + 1, ctx);
            out.push_str(&format!("{i}}}\n"));
        }
        Stmt::While {
            counter,
            limit,
            label,
            body,
        } => {
            out.push_str(&format!("{i}let mut {counter}: u32 = 0u32;\n"));
            out.push_str(&format!("{i}'l{label}: while {counter} < {limit}u32 {{\n"));
            out.push_str(&format!(
                "{}{counter} = {counter}.wrapping_add(1u32);\n",
                ind(l + 1)
            ));
            render_block(out, body, l + 1, ctx);
            out.push_str(&format!("{i}}}\n"));
        }
        Stmt::WhileLet {
            opt,
            bind,
            rebind,
            limit,
            label,
            body,
        } => {
            out.push_str(&format!(
                "{i}'l{label}: while let Some({bind}) = {opt} {{\n"
            ));
            out.push_str(&format!("{}let mut {rebind}: u32 = {bind};\n", ind(l + 1)));
            out.push_str(&format!(
                "{}{opt} = if {rebind} < {limit}u32 {{\n{}Some(({rebind}).wrapping_add(1u32))\n{}}} else {{\n{}None\n{}}};\n",
                ind(l + 1),
                ind(l + 2),
                ind(l + 1),
                ind(l + 2),
                ind(l + 1)
            ));
            render_block(out, body, l + 1, ctx);
            out.push_str(&format!("{i}}}\n"));
        }
        Stmt::Loop {
            counter,
            limit,
            label,
            body,
        } => {
            out.push_str(&format!("{i}let mut {counter}: u32 = 0u32;\n"));
            out.push_str(&format!("{i}'l{label}: loop {{\n"));
            out.push_str(&format!(
                "{}if {counter} >= {limit}u32 {{\n{}break 'l{label};\n{}}}\n",
                ind(l + 1),
                ind(l + 2),
                ind(l + 1)
            ));
            out.push_str(&format!(
                "{}{counter} = {counter}.wrapping_add(1u32);\n",
                ind(l + 1)
            ));
            render_block(out, body, l + 1, ctx);
            out.push_str(&format!("{i}}}\n"));
        }
        Stmt::ValueLoop {
            name,
            counter,
            limit,
            label,
            fuel_value,
            body,
        } => {
            out.push_str(&format!("{i}let mut {counter}: u32 = 0u32;\n"));
            out.push_str(&format!("{i}let mut {name}: u32 = 'l{label}: loop {{\n"));
            out.push_str(&format!(
                "{}if {counter} >= {limit}u32 {{\n{}break 'l{label} {};\n{}}}\n",
                ind(l + 1),
                ind(l + 2),
                expr(fuel_value),
                ind(l + 1)
            ));
            out.push_str(&format!(
                "{}{counter} = {counter}.wrapping_add(1u32);\n",
                ind(l + 1)
            ));
            render_block(out, body, l + 1, ctx);
            out.push_str(&format!("{i}}};\n"));
        }
        Stmt::LabeledBlock { label, body } => {
            out.push_str(&format!("{i}'l{label}: {{\n"));
            render_block(out, body, l + 1, ctx);
            out.push_str(&format!("{i}}}\n"));
        }
        Stmt::Break(label) => {
            out.push_str(&format!("{i}break 'l{label};\n"));
        }
        Stmt::BreakValue(label, e) => {
            out.push_str(&format!("{i}break 'l{label} {};\n", expr(e)));
        }
        Stmt::Continue(label) => {
            out.push_str(&format!("{i}continue 'l{label};\n"));
        }
        Stmt::ContinueBare => {
            out.push_str(&format!("{i}continue;\n"));
        }
        // A unit return renders as the bare `return;` form.
        Stmt::Return(RetExpr::Unit) => {
            out.push_str(&format!("{i}return;\n"));
        }
        Stmt::Return(r) => {
            out.push_str(&format!("{i}return {};\n", ret_expr(r)));
        }
        Stmt::LetIfValue {
            name,
            cond: c,
            then_b,
            then_e,
            else_b,
            else_e,
        } => {
            out.push_str(&format!("{i}let mut {name}: u32 = if {} {{\n", cond(c)));
            render_block(out, then_b, l + 1, ctx);
            out.push_str(&format!("{}{}\n", ind(l + 1), expr(then_e)));
            out.push_str(&format!("{i}}} else {{\n"));
            render_block(out, else_b, l + 1, ctx);
            out.push_str(&format!("{}{}\n", ind(l + 1), expr(else_e)));
            out.push_str(&format!("{i}}};\n"));
        }
        Stmt::LetMatchValue {
            name,
            scrut,
            modulus,
            arms,
        } => {
            out.push_str(&format!(
                "{i}let mut {name}: u32 = match ({}) % {modulus}u32 {{\n",
                expr(scrut)
            ));
            for (j, (guard, arm, e)) in arms.iter().enumerate() {
                let pat = arm_pat(j, *modulus, guard);
                out.push_str(&format!("{}{pat} => {{\n", ind(l + 1)));
                render_block(out, arm, l + 2, ctx);
                out.push_str(&format!("{}{}\n", ind(l + 2), expr(e)));
                out.push_str(&format!("{}}}\n", ind(l + 1)));
            }
            out.push_str(&format!("{i}}};\n"));
        }
        Stmt::LetBorrow {
            name,
            source,
            mutable,
            annot,
        } => {
            let (ty, amp) = if *mutable {
                (": &mut u32", "&mut ")
            } else {
                (": &u32", "&")
            };
            let ty = if *annot { ty } else { "" };
            out.push_str(&format!("{i}let {name}{ty} = {amp}{source};\n"));
        }
        Stmt::LetBorrowChain {
            name,
            source,
            annot,
        } => {
            let ty = if *annot { ": &&u32" } else { "" };
            out.push_str(&format!("{i}let {name}{ty} = &{source};\n"));
        }
        Stmt::DerefWrite { borrow, expr: e } => {
            out.push_str(&format!("{i}*{borrow} = {};\n", expr(e)));
        }
        Stmt::BorrowSpan(stmts) => {
            render_block(out, stmts, l, ctx);
        }
        Stmt::Delegate {
            sub_case,
            sub_shape,
            sub_var,
            args,
            bind,
        } => {
            let call = call_args(*sub_shape, args);
            // `crate::` rather than `super::`: the coroutine transformation
            // moves body code into a nested generated module, which would
            // change what `super` refers to.
            let sub_mod = format!("crate::case_{sub_case:03}");
            match ctx.world {
                World::Coroutine => {
                    out.push_str(&format!(
                        "{i}let {sub_var}: {sub_mod}::co::State = {sub_mod}::co({call});\n"
                    ));
                    match bind {
                        DelegateBind::Discard => {
                            out.push_str(&format!("{i}yield_all!({sub_var});\n"));
                        }
                        DelegateBind::U32(name) => {
                            out.push_str(&format!("{i}let {name}: u32 = yield_all!({sub_var});\n"));
                        }
                        DelegateBind::Opt(name) => {
                            out.push_str(&format!(
                                "{i}let mut {name}: Option<u32> = yield_all!({sub_var});\n"
                            ));
                        }
                        DelegateBind::Res(name) => {
                            out.push_str(&format!(
                                "{i}let {name}: Result<u32, baregen_difftest::Err1> = yield_all!({sub_var});\n"
                            ));
                        }
                    }
                }
                World::Reference => {
                    let call = format!("{sub_mod}::reference({call})");
                    match bind {
                        DelegateBind::Discard => {
                            out.push_str(&format!("{i}{call};\n"));
                        }
                        DelegateBind::U32(name) => {
                            out.push_str(&format!("{i}let {name}: u32 = {call};\n"));
                        }
                        DelegateBind::Opt(name) => {
                            out.push_str(&format!("{i}let mut {name}: Option<u32> = {call};\n"));
                        }
                        DelegateBind::Res(name) => {
                            out.push_str(&format!(
                                "{i}let {name}: Result<u32, baregen_difftest::Err1> = {call};\n"
                            ));
                        }
                    }
                }
            }
        }
    }
}

/// Formats a delegation call's argument list to match the sub-case's
/// signature shape.
fn call_args(shape: ArgShape, args: &[Expr]) -> String {
    let a: Vec<String> = args.iter().map(expr).collect();
    match shape {
        ArgShape::Plain => format!("{}, {}", a[0], a[1]),
        ArgShape::Tuple => format!("({}, {})", a[0], a[1]),
        ArgShape::Mixed => format!("({}, {}), {}", a[0], a[1], a[2]),
    }
}

fn expr(e: &Expr) -> String {
    match e {
        Expr::Lit(v) => format!("{v}u32"),
        Expr::LitUn(v) => format!("{v}"),
        Expr::Var(n) => n.clone(),
        Expr::WrapAdd(a, b) => format!("({}).wrapping_add({})", expr_typed(a), expr(b)),
        Expr::WrapSub(a, b) => format!("({}).wrapping_sub({})", expr_typed(a), expr(b)),
        Expr::WrapMul(a, b) => format!("({}).wrapping_mul({})", expr_typed(a), expr(b)),
        Expr::Rem(a, m) => format!("(({}) % {m}u32)", expr(a)),
        // The operand is reduced below 16, so the i32 negation cannot
        // overflow; `as u32` wraps and never panics.
        Expr::NegCast(a) => format!("((-((({}) % 16u32) as i32)) as u32)", expr(a)),
        Expr::CastRound(a) => format!("((({}) as u64) as u32)", expr(a)),
        Expr::TupleField(a, b, idx) => format!("(({}, {}).{idx})", expr(a), expr(b)),
        Expr::PairField { x, y, second } => {
            let field = if *second { "y" } else { "x" };
            format!(
                "(baregen_difftest::Pair {{ x: {}, y: {} }}.{field})",
                expr(x),
                expr(y)
            )
        }
        Expr::Index { arr, idx } => format!("({arr}[((({}) % 4u32) as usize)])", expr(idx)),
        Expr::Deref { name, count } => format!("({}{name})", "*".repeat(*count as usize)),
    }
}

/// Like [`expr`], for positions whose type rustc will not infer from
/// context (the receiver of a `wrapping_*` method call): unsuffixed
/// literals are rendered suffixed, including one selected through a
/// tuple field access, so the receiver type is never ambiguous.
fn expr_typed(e: &Expr) -> String {
    match e {
        Expr::LitUn(v) => format!("{v}u32"),
        Expr::TupleField(a, b, idx) => {
            let (a, b) = if *idx == 0 {
                (expr_typed(a), expr(b))
            } else {
                (expr(a), expr_typed(b))
            };
            format!("(({a}, {b}).{idx})")
        }
        _ => expr(e),
    }
}

fn ret_expr(r: &RetExpr) -> String {
    match r {
        RetExpr::Plain(e) => expr(e),
        RetExpr::Wrapped(e) => format!("Some({})", expr(e)),
        RetExpr::NoneLit => "None".to_string(),
        RetExpr::OptVar(n) => n.clone(),
        RetExpr::OkWrapped(e) => format!("Ok({})", expr(e)),
        RetExpr::ErrWrapped(e) => format!("Err(baregen_difftest::Err1({}))", expr(e)),
        RetExpr::Unit => "()".to_string(),
    }
}

/// `yield_!(e)` — or the argument-less `yield_!()` when the case's
/// yield type is `()`.
fn yc(e: &Expr, ctx: Ctx) -> String {
    if ctx.unit_yield {
        "yield_!()".to_string()
    } else {
        format!("yield_!({})", expr(e))
    }
}

/// Pattern (plus optional pure guard) of match arm `j`. The trailing
/// `_` arm is never guarded.
fn arm_pat(j: usize, modulus: u32, guard: &Option<Cond>) -> String {
    let pat = if j + 1 == modulus as usize {
        "_".to_string()
    } else {
        format!("{j}u32")
    };
    match guard {
        Some(c) => format!("{pat} if {}", cond(c)),
        None => pat,
    }
}

fn cond(c: &Cond) -> String {
    match c {
        Cond::Lt(a, b) => format!("({}) < ({})", expr(a), expr(b)),
        Cond::ModIsZero(e, m) => format!("({}) % {m}u32 == 0u32", expr(e)),
        Cond::And(a, b) => format!("({}) && ({})", cond(a), cond(b)),
        Cond::Or(a, b) => format!("({}) || ({})", cond(a), cond(b)),
    }
}
