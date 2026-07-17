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

pub fn render_case(idx: usize, case: &Case) -> String {
    let name = format!("case_{idx:03}");
    let ret_ty = match case.flavor {
        Flavor::U32 => "u32",
        Flavor::OptionU32 => "Option<u32>",
        // Fully qualified: the coroutine attribute moves the body into a
        // generated module, so bare imported names would not resolve.
        Flavor::ResultU32 => "Result<u32, baregen_difftest::Err1>",
    };
    let attr = if case.fingerprint {
        "#[baregen::coroutine(yield = u32, resume = u32, fingerprint)]"
    } else {
        "#[baregen::coroutine(yield = u32, resume = u32)]"
    };
    let co_body = render_body(&case.body, 2, World::Coroutine);
    let ref_body = render_body(&case.body, 2, World::Reference);
    let source = format!(
        "{attr}\nfn co(a0: u32, a1: u32) -> {ret_ty} {{\n{}}}",
        render_body(&case.body, 1, World::Coroutine)
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
    pub fn co(a0: u32, a1: u32) -> {ret_ty} {{
{co_body}    }}

    pub fn reference(a0: u32, a1: u32) -> {ret_ty} {{
{ref_body}    }}
}}

proptest::proptest! {{
    #![proptest_config(baregen_difftest::proptest_config())]
    #[test]
    fn {name}(
        a0 in 0u32..16u32,
        a1 in 0u32..16u32,
        resumes in proptest::collection::vec(0u32..16u32, 1..12),
    ) {{
        baregen_difftest::check_case(
            {name}::SOURCE,
            &[a0, a1],
            &resumes,
            || {name}::reference(a0, a1),
            {name}::co(a0, a1),
        );
    }}
}}
"####
    )
}

fn ind(level: usize) -> String {
    "    ".repeat(level)
}

fn render_body(body: &Body, level: usize, world: World) -> String {
    let mut out = String::new();
    for s in &body.stmts {
        render_stmt(&mut out, s, level, world);
    }
    let i = ind(level);
    match &body.tail {
        Tail::Ret(r) => out.push_str(&format!("{i}{}\n", ret_expr(r))),
        Tail::Yield(e) => out.push_str(&format!("{i}yield_!({})\n", expr(e))),
        Tail::YieldWrapped(e) => out.push_str(&format!("{i}Some(yield_!({}))\n", expr(e))),
        Tail::YieldOk(e) => out.push_str(&format!("{i}Ok(yield_!({}))\n", expr(e))),
    }
    out
}

fn render_block(out: &mut String, stmts: &[Stmt], level: usize, world: World) {
    for s in stmts {
        render_stmt(out, s, level, world);
    }
}

fn render_stmt(out: &mut String, s: &Stmt, l: usize, world: World) {
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
            out.push_str(&format!("{i}yield_!({});\n", expr(e)));
        }
        Stmt::LetYield { name, arg } => {
            out.push_str(&format!("{i}let {name} = yield_!({});\n", expr(arg)));
        }
        Stmt::LetYieldAdd { name, a, b } => {
            out.push_str(&format!(
                "{i}let {name}: u32 = u32::wrapping_add(yield_!({}), yield_!({}));\n",
                expr(a),
                expr(b)
            ));
        }
        Stmt::AssignYieldAdd { name, arg } => {
            out.push_str(&format!(
                "{i}{name} = u32::wrapping_add({name}, yield_!({}));\n",
                expr(arg)
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
            else_b,
        } => {
            out.push_str(&format!("{i}if {} {{\n", cond(c)));
            render_block(out, then_b, l + 1, world);
            if let Some(eb) = else_b {
                out.push_str(&format!("{i}}} else {{\n"));
                render_block(out, eb, l + 1, world);
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
            render_block(out, then_b, l + 1, world);
            if let Some(eb) = else_b {
                out.push_str(&format!("{i}}} else {{\n"));
                render_block(out, eb, l + 1, world);
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
                render_block(out, arm, l + 2, world);
                out.push_str(&format!("{}}}\n", ind(l + 1)));
            }
            out.push_str(&format!("{i}}}\n"));
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
            render_block(out, body, l + 1, world);
            out.push_str(&format!("{i}}};\n"));
        }
        Stmt::For {
            var,
            upper,
            label,
            body,
        } => {
            let up = match upper {
                Upper::Lit(n) => format!("{n}u32"),
                Upper::Var(v) => v.clone(),
            };
            out.push_str(&format!("{i}'l{label}: for {var} in 0u32..{up} {{\n"));
            render_block(out, body, l + 1, world);
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
            render_block(out, body, l + 1, world);
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
            render_block(out, body, l + 1, world);
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
            render_block(out, body, l + 1, world);
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
            render_block(out, body, l + 1, world);
            out.push_str(&format!("{i}}};\n"));
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
            render_block(out, then_b, l + 1, world);
            out.push_str(&format!("{}{}\n", ind(l + 1), expr(then_e)));
            out.push_str(&format!("{i}}} else {{\n"));
            render_block(out, else_b, l + 1, world);
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
                render_block(out, arm, l + 2, world);
                out.push_str(&format!("{}{}\n", ind(l + 2), expr(e)));
                out.push_str(&format!("{}}}\n", ind(l + 1)));
            }
            out.push_str(&format!("{i}}};\n"));
        }
        Stmt::Delegate {
            sub_case,
            sub_var,
            args,
            bind,
        } => {
            let (e1, e2) = (expr(&args.0), expr(&args.1));
            // `crate::` rather than `super::`: the coroutine transformation
            // moves body code into a nested generated module, which would
            // change what `super` refers to.
            let sub_mod = format!("crate::case_{sub_case:03}");
            match world {
                World::Coroutine => {
                    out.push_str(&format!(
                        "{i}let {sub_var}: {sub_mod}::co::State = {sub_mod}::co({e1}, {e2});\n"
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
                    let call = format!("{sub_mod}::reference({e1}, {e2})");
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

fn expr(e: &Expr) -> String {
    match e {
        Expr::Lit(v) => format!("{v}u32"),
        Expr::Var(n) => n.clone(),
        Expr::WrapAdd(a, b) => format!("({}).wrapping_add({})", expr(a), expr(b)),
        Expr::WrapSub(a, b) => format!("({}).wrapping_sub({})", expr(a), expr(b)),
        Expr::WrapMul(a, b) => format!("({}).wrapping_mul({})", expr(a), expr(b)),
        Expr::Rem(a, m) => format!("(({}) % {m}u32)", expr(a)),
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
    }
}
