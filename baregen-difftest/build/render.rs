//! Renders a generated body into a test-case module. The body text is
//! rendered once and pasted into both the coroutine and the reference
//! function, so the two worlds can never diverge textually.

use crate::ast::*;

pub fn render_case(idx: usize, body: &Body) -> String {
    let name = format!("case_{idx:03}");
    let body_at_1 = render_body(body, 1);
    let body_at_2 = render_body(body, 2);
    let source = format!(
        "#[baregen::coroutine(yield = u32, resume = u32)]\nfn co(a0: u32, a1: u32) -> u32 {{\n{body_at_1}}}"
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
    unreachable_code,
    dead_code,
    clippy::all
)]
mod {name} {{
    use baregen_difftest::yield_;

    pub const SOURCE: &str = r##"{source}"##;

    #[baregen::coroutine(yield = u32, resume = u32)]
    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    pub fn co(a0: u32, a1: u32) -> u32 {{
{body_at_2}    }}

    pub fn reference(a0: u32, a1: u32) -> u32 {{
{body_at_2}    }}
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

fn render_body(body: &Body, level: usize) -> String {
    let mut out = String::new();
    for s in &body.stmts {
        render_stmt(&mut out, s, level);
    }
    match &body.tail {
        Tail::Expr(e) => out.push_str(&format!("{}{}\n", ind(level), expr(e))),
        Tail::Yield(e) => out.push_str(&format!("{}yield_!({})\n", ind(level), expr(e))),
    }
    out
}

fn render_block(out: &mut String, stmts: &[Stmt], level: usize) {
    for s in stmts {
        render_stmt(out, s, level);
    }
}

fn render_stmt(out: &mut String, s: &Stmt, l: usize) {
    let i = ind(l);
    match s {
        Stmt::Let { name, expr: e } => {
            out.push_str(&format!("{i}let mut {name}: u32 = {};\n", expr(e)));
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
        Stmt::If {
            cond: c,
            then_b,
            else_b,
        } => {
            out.push_str(&format!("{i}if {} {{\n", cond(c)));
            render_block(out, then_b, l + 1);
            if let Some(eb) = else_b {
                out.push_str(&format!("{i}}} else {{\n"));
                render_block(out, eb, l + 1);
            }
            out.push_str(&format!("{i}}}\n"));
        }
        Stmt::Match {
            scrut,
            modulus,
            arms,
        } => {
            out.push_str(&format!("{i}match ({}) % {modulus}u32 {{\n", expr(scrut)));
            for (j, arm) in arms.iter().enumerate() {
                let pat = if j + 1 == *modulus as usize {
                    "_".to_string()
                } else {
                    format!("{j}u32")
                };
                out.push_str(&format!("{}{pat} => {{\n", ind(l + 1)));
                render_block(out, arm, l + 2);
                out.push_str(&format!("{}}}\n", ind(l + 1)));
            }
            out.push_str(&format!("{i}}}\n"));
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
            render_block(out, body, l + 1);
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
            render_block(out, body, l + 1);
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
            render_block(out, body, l + 1);
            out.push_str(&format!("{i}}}\n"));
        }
        Stmt::Break(label) => {
            out.push_str(&format!("{i}break 'l{label};\n"));
        }
        Stmt::Continue(label) => {
            out.push_str(&format!("{i}continue 'l{label};\n"));
        }
        Stmt::Return(e) => {
            out.push_str(&format!("{i}return {};\n", expr(e)));
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
            render_block(out, then_b, l + 1);
            out.push_str(&format!("{}{}\n", ind(l + 1), expr(then_e)));
            out.push_str(&format!("{i}}} else {{\n"));
            render_block(out, else_b, l + 1);
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
            for (j, (arm, e)) in arms.iter().enumerate() {
                let pat = if j + 1 == *modulus as usize {
                    "_".to_string()
                } else {
                    format!("{j}u32")
                };
                out.push_str(&format!("{}{pat} => {{\n", ind(l + 1)));
                render_block(out, arm, l + 2);
                out.push_str(&format!("{}{}\n", ind(l + 2), expr(e)));
                out.push_str(&format!("{}}}\n", ind(l + 1)));
            }
            out.push_str(&format!("{i}}};\n"));
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

fn cond(c: &Cond) -> String {
    match c {
        Cond::Lt(a, b) => format!("({}) < ({})", expr(a), expr(b)),
        Cond::ModIsZero(e, m) => format!("({}) % {m}u32 == 0u32", expr(e)),
    }
}
