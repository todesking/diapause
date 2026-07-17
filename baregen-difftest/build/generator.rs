//! Random generation of supported coroutine bodies.
//!
//! Validity is guaranteed by construction:
//! - every `let` is annotated (syntactic-type rule), names are globally
//!   fresh (no shadowing, so opaque jumps are always legal), and
//!   pattern bindings (`if let` / `while let`) are rebound immediately
//!   so they never cross a yield;
//! - loops are fuel-bounded (`for` over bounded ranges, counters
//!   incremented before the body, `while let` scrutinees strictly
//!   increasing toward a literal limit), so every body terminates;
//! - a conditional gets at most one branch ending in a jump, so the
//!   trailing expression is always statically reachable; a `let else`
//!   block always ends in a jump, so it always diverges;
//! - inside value-position expressions (`let x = if/match/loop`) no
//!   `return` is generated and enclosing loop labels are hidden, so
//!   jumps never escape a value expression;
//! - all arithmetic is wrapping and `%` uses non-zero literals, so no
//!   generated program can panic;
//! - `yield_all!` targets only earlier cases, and every case carries a
//!   static worst-case yield bound; a delegation (even inside loops or
//!   chained through other delegating cases) is generated only while
//!   the current case's bound stays under [`YIELD_BOUND`], so trace
//!   lengths cannot run away; its arguments are reduced mod 16 so
//!   argument-bounded loops in the sub-case stay small.

use crate::ast::*;
use crate::rng::Rng;

/// Cap on a case's static worst-case yield count when adding a
/// delegation. The harness aborts a run at MAX_YIELDS = 10_000
/// (src/lib.rs), so 2_000 leaves a 5x margin; real traces stay far
/// below the worst case because arguments, branches, and early jumps
/// cut loops short, and the proptest resume script only steers resume
/// values, never adds yields.
const YIELD_BOUND: u64 = 2_000;

/// What later cases need to know about an earlier one to delegate to it.
pub struct PriorCase {
    pub flavor: Flavor,
    pub bound: u64,
}

pub fn generate(seed: u64, index: usize, prior: &[PriorCase]) -> Case {
    let case_seed = seed
        ^ (index as u64)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(0x5EED);
    Gen::new(Rng::new(case_seed), prior).gen_case()
}

struct Var {
    name: String,
    /// Only `let mut` u32 bindings may be assignment targets; loop
    /// variables and counters are read-only to preserve termination.
    assignable: bool,
    /// `Option<u32>` variables live in the same scope stack but are
    /// only used as `if let` / `while let` / `?` operands.
    opt: bool,
    /// `Result<u32, _>` variables are only used as `?` operands.
    res: bool,
    /// `[u32; 4]` variables are only used as `Expr::Index` bases.
    arr: bool,
}

struct LoopLabel {
    id: usize,
    /// Labels of `ValueLoop`s: breaks targeting them carry a value.
    value: bool,
    /// Labels of `LabeledBlock`s: only plain `break` may target them,
    /// and an unlabeled `continue` skips them.
    block: bool,
}

struct Gen<'a> {
    rng: Rng,
    prior: &'a [PriorCase],
    flavor: Flavor,
    scope: Vec<Var>,
    loop_labels: Vec<LoopLabel>,
    next_var: usize,
    next_label: usize,
    depth: usize,
    yields: usize,
    /// Product of the iteration bounds of all enclosing loops.
    loop_mult: u64,
    /// Worst-case yields of the statements generated so far.
    bound: u64,
    in_value: bool,
}

impl<'a> Gen<'a> {
    fn new(mut rng: Rng, prior: &'a [PriorCase]) -> Self {
        let flavor = match rng.below(4) {
            0 => Flavor::OptionU32,
            1 => Flavor::ResultU32,
            _ => Flavor::U32,
        };
        Gen {
            rng,
            prior,
            flavor,
            scope: vec![
                Var {
                    name: "a0".into(),
                    assignable: false,
                    opt: false,
                    res: false,
                    arr: false,
                },
                Var {
                    name: "a1".into(),
                    assignable: false,
                    opt: false,
                    res: false,
                    arr: false,
                },
            ],
            loop_labels: Vec::new(),
            next_var: 0,
            next_label: 0,
            depth: 0,
            yields: 0,
            loop_mult: 1,
            bound: 0,
            in_value: false,
        }
    }

    fn gen_case(mut self) -> Case {
        let fingerprint = self.rng.chance(1, 4);
        let mut budget = 4 + self.rng.below(10) as i32;
        let stmts = self.gen_block(&mut budget);
        let tail = self.gen_tail();
        Case {
            body: Body { stmts, tail },
            flavor: self.flavor,
            fingerprint,
            bound: self.bound,
        }
    }

    fn gen_tail(&mut self) -> Tail {
        // Tail-position delegation: the sub-case's completion value is
        // this coroutine's, so it must have the same flavor.
        if self.rng.chance(1, 8) {
            let candidates: Vec<usize> = self
                .prior
                .iter()
                .enumerate()
                .filter(|(_, p)| p.flavor == self.flavor && self.delegation_fits(p.bound))
                .map(|(i, _)| i)
                .collect();
            if !candidates.is_empty() {
                let sub_case = *self.rng.pick(&candidates);
                self.bound += self.prior[sub_case].bound;
                let args = (
                    Expr::Rem(Box::new(self.gen_expr(1)), 16),
                    Expr::Rem(Box::new(self.gen_expr(1)), 16),
                );
                let sub_var = self.fresh("s");
                return Tail::Delegate {
                    sub_case,
                    sub_var,
                    args,
                };
            }
        }
        if self.rng.chance(1, 7) {
            return self.gen_tail_break_loop();
        }
        if self.yields == 0 || self.rng.chance(3, 10) {
            self.count_yields(1);
            let e = self.gen_expr(2);
            match self.flavor {
                Flavor::U32 => Tail::Yield(e),
                Flavor::OptionU32 => Tail::YieldWrapped(e),
                Flavor::ResultU32 => Tail::YieldOk(e),
            }
        } else {
            Tail::Ret(self.gen_ret_expr())
        }
    }

    /// A value-bearing fuel loop as the function's trailing expression.
    /// Like a `ValueLoop`, the loop is a value expression: enclosing
    /// labels are hidden (there are none at the tail anyway) and
    /// `return` is banned inside; only for the U32 flavor is its own
    /// label jumpable, because generated `break`-values are u32.
    fn gen_tail_break_loop(&mut self) -> Tail {
        let counter = self.fresh("c");
        let limit = 1 + self.rng.below(3) as u32;
        let label = self.fresh_label();
        self.push_var(&counter, false, false);
        let fuel = self.gen_ret_expr();
        let prev_in_value = self.in_value;
        self.in_value = true;
        let saved_labels = std::mem::take(&mut self.loop_labels);
        if self.flavor == Flavor::U32 {
            self.loop_labels.push(LoopLabel {
                id: label,
                value: true,
                block: false,
            });
        }
        self.depth += 1;
        let mut budget = 2 + self.rng.below(4) as i32;
        let mut body = self.gen_loop_body(limit as u64, &mut budget);
        self.depth -= 1;
        self.loop_labels = saved_labels;
        self.in_value = prev_in_value;
        if self.yields == 0 {
            // Keep the every-case-yields invariant even when neither
            // the statements nor the loop body yielded.
            let e = self.gen_expr(1);
            let saved = self.loop_mult;
            self.loop_mult *= limit as u64;
            self.count_yields(1);
            self.loop_mult = saved;
            body.insert(0, Stmt::Yield(e));
        }
        Tail::BreakLoop {
            counter,
            limit,
            label,
            fuel,
            body,
        }
    }

    /// Records `n` yields at the current loop-nesting multiplier.
    fn count_yields(&mut self, n: u64) {
        self.yields += n as usize;
        self.bound += self.loop_mult * n;
    }

    /// Whether delegating to a case with the given bound keeps this
    /// case's worst-case yield count under [`YIELD_BOUND`].
    fn delegation_fits(&self, sub_bound: u64) -> bool {
        self.bound + self.loop_mult * sub_bound <= YIELD_BOUND
    }

    fn fresh(&mut self, prefix: &str) -> String {
        let n = format!("{prefix}{}", self.next_var);
        self.next_var += 1;
        n
    }

    fn fresh_label(&mut self) -> usize {
        let l = self.next_label;
        self.next_label += 1;
        l
    }

    fn gen_block(&mut self, budget: &mut i32) -> Vec<Stmt> {
        let mark = self.scope.len();
        let mut stmts = Vec::new();
        loop {
            if *budget <= 0 {
                break;
            }
            stmts.push(self.gen_stmt(budget));
            let stop_num = if self.depth == 0 { 1 } else { 2 };
            if self.rng.chance(stop_num, 5) {
                break;
            }
        }
        self.scope.truncate(mark);
        stmts
    }

    /// A block plus its trailing value expression, generated while the
    /// block's bindings are still in scope.
    fn gen_block_with_tail(&mut self, budget: &mut i32) -> (Vec<Stmt>, Expr) {
        let mark = self.scope.len();
        let mut stmts = Vec::new();
        while *budget > 0 && self.rng.chance(3, 5) {
            stmts.push(self.gen_stmt(budget));
        }
        let e = self.gen_expr(2);
        self.scope.truncate(mark);
        (stmts, e)
    }

    fn has_opt_var(&self) -> bool {
        self.scope.iter().any(|v| v.opt)
    }

    fn has_res_var(&self) -> bool {
        self.scope.iter().any(|v| v.res)
    }

    /// Whether a diverging jump can be generated here (needed as the
    /// mandatory terminator of a `let else` block).
    fn jump_possible(&self) -> bool {
        !self.in_value || !self.loop_labels.is_empty()
    }

    fn gen_stmt(&mut self, budget: &mut i32) -> Stmt {
        *budget -= 1;
        let structural = self.depth < 4 && *budget > 0;
        let has_assignable = self.scope.iter().any(|v| v.assignable && !v.opt);
        let has_opt = self.has_opt_var();
        let has_res = self.has_res_var();
        // Delegation anywhere (loop bodies, value positions, chains
        // through other delegating cases) as long as the static yield
        // bound stays under YIELD_BOUND.
        let delegatable = self.prior.iter().any(|p| self.delegation_fits(p.bound));

        let mut kinds: Vec<(u64, u8)> = vec![(3, 0), (3, 1), (2, 2), (1, 3), (2, 11)];
        kinds.push((1, 20));
        kinds.push((2, 21));
        kinds.push((1, 23));
        kinds.push((1, 25));
        if has_assignable {
            kinds.push((2, 4));
            kinds.push((1, 5));
            kinds.push((1, 22));
        }
        if has_opt && self.flavor == Flavor::OptionU32 {
            kinds.push((2, 15));
        }
        // Weighted higher than the Option counterparts: the `?`-with-
        // From-conversion path is the whole point of this flavor.
        if self.flavor == Flavor::ResultU32 {
            kinds.push((4, 18));
            if has_res {
                kinds.push((6, 19));
            }
        }
        if structural {
            kinds.push((3, 6));
            kinds.push((2, 7));
            kinds.push((3, 8));
            kinds.push((1, 9));
            kinds.push((1, 10));
            kinds.push((1, 14));
            if has_opt {
                kinds.push((2, 12));
                kinds.push((2, 13));
            }
            if self.jump_possible() {
                kinds.push((2, 16));
            }
            kinds.push((1, 26));
        }
        if delegatable {
            kinds.push((2, 17));
        }

        let total: u64 = kinds.iter().map(|k| k.0).sum();
        let mut roll = self.rng.below(total);
        let mut kind = 0u8;
        for (w, k) in kinds {
            if roll < w {
                kind = k;
                break;
            }
            roll -= w;
        }
        match kind {
            0 => {
                let expr = self.gen_expr(2);
                let name = self.fresh("v");
                self.push_var(&name, true, false);
                Stmt::Let { name, expr }
            }
            1 => {
                self.count_yields(1);
                Stmt::Yield(self.gen_expr(2))
            }
            2 => {
                self.count_yields(1);
                let arg = self.gen_expr(2);
                let annot = self.rng.chance(1, 3);
                let name = self.fresh("r");
                self.push_var(&name, false, false);
                Stmt::LetYield { name, arg, annot }
            }
            3 => {
                self.count_yields(2);
                let a = self.gen_expr(1);
                let b = self.gen_expr(1);
                let name = self.fresh("r");
                self.push_var(&name, false, false);
                Stmt::LetYieldAdd { name, a, b }
            }
            4 => Stmt::Assign {
                name: self.pick_assignable(),
                expr: self.gen_expr(2),
            },
            5 => {
                self.count_yields(1);
                Stmt::AssignYieldAdd {
                    name: self.pick_assignable(),
                    arg: self.gen_expr(1),
                }
            }
            6 => self.gen_if(budget),
            7 => self.gen_match(budget),
            8 => self.gen_loop(budget),
            9 => self.gen_let_if_value(budget),
            10 => self.gen_let_match_value(budget),
            11 => {
                let init = if self.rng.chance(3, 4) {
                    Some(self.gen_expr(2))
                } else {
                    None
                };
                let name = self.fresh("o");
                self.push_var(&name, false, true);
                Stmt::LetOption { name, init }
            }
            12 => self.gen_if_let(budget),
            13 => self.gen_while_let(budget),
            14 => self.gen_value_loop(budget),
            15 => {
                let opt = self.pick_opt_var();
                let name = self.fresh("v");
                self.push_var(&name, false, false);
                Stmt::LetTry { name, opt }
            }
            16 => self.gen_let_else(budget),
            17 => self.gen_delegate(),
            18 => {
                let e = self.gen_expr(2);
                let init = if self.rng.chance(3, 4) { Ok(e) } else { Err(e) };
                let name = self.fresh("q");
                self.push_res_var(&name);
                Stmt::LetResult { name, init }
            }
            19 => {
                let res = self.pick_res_var();
                let name = self.fresh("v");
                self.push_var(&name, false, false);
                Stmt::LetTryResult { name, res }
            }
            20 => {
                let elems = (0..4).map(|_| self.gen_expr(1)).collect();
                let name = self.fresh("arr");
                self.push_arr_var(&name);
                Stmt::LetArray { name, elems }
            }
            21 => self.gen_match_yield(),
            22 => {
                self.count_yields(1);
                Stmt::XorAssignYield {
                    name: self.pick_assignable(),
                    arg: self.gen_expr(1),
                }
            }
            23 => {
                let from = self.pick_u32_var();
                let name = self.fresh("m");
                self.push_var(&name, true, false);
                Stmt::LetInfer { name, from }
            }
            25 => {
                // The i32 binding is deliberately kept out of the u32
                // variable pool and never referenced again.
                let name = self.fresh("n");
                Stmt::LetNegLit {
                    name,
                    val: self.rng.below(16) as u32,
                }
            }
            _ => self.gen_labeled_block(budget),
        }
    }

    fn gen_labeled_block(&mut self, budget: &mut i32) -> Stmt {
        let label = self.fresh_label();
        self.depth += 1;
        self.loop_labels.push(LoopLabel {
            id: label,
            value: false,
            block: true,
        });
        let body = self.gen_block(budget);
        self.loop_labels.pop();
        self.depth -= 1;
        Stmt::LabeledBlock { label, body }
    }

    /// A u32 variable usable in plain expressions (arguments always
    /// qualify, so the pool is never empty).
    fn pick_u32_var(&mut self) -> String {
        let names: Vec<&str> = self
            .scope
            .iter()
            .filter(|v| !v.opt && !v.res && !v.arr)
            .map(|v| v.name.as_str())
            .collect();
        (*self.rng.pick(&names)).to_string()
    }

    /// A statement-position `match` with non-block arm bodies; some
    /// arms are `yield_!(e)`, the rest are pure expressions. Exactly
    /// one arm runs, so at most one yield is counted.
    fn gen_match_yield(&mut self) -> Stmt {
        let scrut = self.gen_expr(2);
        let modulus = 2 + self.rng.below(2) as u32;
        let arms: Vec<(Option<Cond>, bool, Expr)> = (0..modulus)
            .map(|j| {
                let guard = self.gen_arm_guard(j, modulus);
                let yields = self.rng.chance(3, 5);
                (guard, yields, self.gen_expr(1))
            })
            .collect();
        if arms.iter().any(|a| a.1) {
            self.count_yields(1);
        }
        Stmt::MatchYield {
            scrut,
            modulus,
            arms,
        }
    }

    fn push_var(&mut self, name: &str, assignable: bool, opt: bool) {
        self.scope.push(Var {
            name: name.to_string(),
            assignable,
            opt,
            res: false,
            arr: false,
        });
    }

    fn push_res_var(&mut self, name: &str) {
        self.scope.push(Var {
            name: name.to_string(),
            assignable: false,
            opt: false,
            res: true,
            arr: false,
        });
    }

    fn push_arr_var(&mut self, name: &str) {
        self.scope.push(Var {
            name: name.to_string(),
            assignable: false,
            opt: false,
            res: false,
            arr: true,
        });
    }

    fn pick_arr_var(&mut self) -> String {
        let names: Vec<&str> = self
            .scope
            .iter()
            .filter(|v| v.arr)
            .map(|v| v.name.as_str())
            .collect();
        (*self.rng.pick(&names)).to_string()
    }

    fn pick_assignable(&mut self) -> String {
        let names: Vec<&str> = self
            .scope
            .iter()
            .filter(|v| v.assignable && !v.opt)
            .map(|v| v.name.as_str())
            .collect();
        (*self.rng.pick(&names)).to_string()
    }

    fn pick_opt_var(&mut self) -> String {
        let names: Vec<&str> = self
            .scope
            .iter()
            .filter(|v| v.opt)
            .map(|v| v.name.as_str())
            .collect();
        (*self.rng.pick(&names)).to_string()
    }

    fn pick_res_var(&mut self) -> String {
        let names: Vec<&str> = self
            .scope
            .iter()
            .filter(|v| v.res)
            .map(|v| v.name.as_str())
            .collect();
        (*self.rng.pick(&names)).to_string()
    }

    fn gen_if(&mut self, budget: &mut i32) -> Stmt {
        let cond = self.gen_cond();
        self.depth += 1;
        let mut then_b = self.gen_block(budget);
        let mut else_ifs: Vec<(Cond, Vec<Stmt>)> = Vec::new();
        while *budget > 0 && else_ifs.len() < 2 && self.rng.chance(1, 4) {
            let c = self.gen_cond();
            let b = self.gen_block(budget);
            else_ifs.push((c, b));
        }
        let mut else_b = if self.rng.chance(1, 2) {
            Some(self.gen_block(budget))
        } else {
            None
        };
        self.depth -= 1;
        // At most one branch may end in a diverging jump, so a
        // fall-through path always remains.
        if self.rng.chance(1, 4)
            && let Some(jump) = self.gen_jump()
        {
            let n = 1 + else_ifs.len() + usize::from(else_b.is_some());
            let k = self.rng.below(n as u64) as usize;
            if k == 0 {
                then_b.push(jump);
            } else if k <= else_ifs.len() {
                else_ifs[k - 1].1.push(jump);
            } else {
                else_b.as_mut().expect("k counted else_b").push(jump);
            }
        }
        Stmt::If {
            cond,
            then_b,
            else_ifs,
            else_b,
        }
    }

    fn gen_if_let(&mut self, budget: &mut i32) -> Stmt {
        let opt = self.pick_opt_var();
        let bind = self.fresh("p");
        let rebind = self.fresh("x");
        self.depth += 1;
        let mark = self.scope.len();
        self.push_var(&rebind, true, false);
        let mut then_b = self.gen_block(budget);
        self.scope.truncate(mark);
        let mut else_b = if self.rng.chance(1, 2) {
            Some(self.gen_block(budget))
        } else {
            None
        };
        self.depth -= 1;
        self.maybe_append_jump(&mut then_b, &mut else_b);
        Stmt::IfLet {
            opt,
            bind,
            rebind,
            then_b,
            else_b,
        }
    }

    /// With some probability, append a diverging jump to at most one
    /// branch of a conditional, so a fall-through path always remains.
    fn maybe_append_jump(&mut self, then_b: &mut Vec<Stmt>, else_b: &mut Option<Vec<Stmt>>) {
        if self.rng.chance(1, 4)
            && let Some(jump) = self.gen_jump()
        {
            match (else_b.as_mut(), self.rng.chance(1, 2)) {
                (Some(b), true) => b.push(jump),
                _ => then_b.push(jump),
            }
        }
    }

    fn gen_match(&mut self, budget: &mut i32) -> Stmt {
        let scrut = self.gen_expr(2);
        let modulus = 2 + self.rng.below(2) as u32;
        self.depth += 1;
        let mut arms: Vec<(Option<Cond>, Vec<Stmt>)> = (0..modulus)
            .map(|j| (self.gen_arm_guard(j, modulus), self.gen_block(budget)))
            .collect();
        self.depth -= 1;
        if self.rng.chance(1, 4)
            && let Some(jump) = self.gen_jump()
        {
            let i = self.rng.below(modulus as u64) as usize;
            arms[i].1.push(jump);
        }
        Stmt::Match {
            scrut,
            modulus,
            arms,
        }
    }

    fn gen_let_else(&mut self, budget: &mut i32) -> Stmt {
        let scrut = self.gen_expr(2);
        let modulus = 2 + self.rng.below(3) as u32;
        self.depth += 1;
        let mut body = self.gen_block(budget);
        self.depth -= 1;
        let jump = self
            .gen_jump()
            .expect("gen_let_else called where no jump is possible");
        body.push(jump);
        Stmt::LetElse {
            scrut,
            modulus,
            body,
        }
    }

    /// Generates a loop body with the loop's iteration bound folded
    /// into the yield-bound multiplier.
    fn gen_loop_body(&mut self, iters: u64, budget: &mut i32) -> Vec<Stmt> {
        let saved = self.loop_mult;
        self.loop_mult *= iters;
        let body = self.gen_block(budget);
        self.loop_mult = saved;
        body
    }

    fn gen_loop(&mut self, budget: &mut i32) -> Stmt {
        let label = self.fresh_label();
        self.depth += 1;
        self.loop_labels.push(LoopLabel {
            id: label,
            value: false,
            block: false,
        });
        let stmt = match self.rng.below(3) {
            0 => {
                let var = self.fresh("i");
                let upper = if self.rng.chance(2, 5) {
                    Upper::Var(if self.rng.chance(1, 2) { "a0" } else { "a1" }.to_string())
                } else {
                    Upper::Lit(1 + self.rng.below(4) as u32)
                };
                // KNOWN BUG, generation disabled: serde round-trip of a
                // state suspended after an inclusive range's last
                // element re-yields that element forever (serde's
                // RangeInclusive impl drops the internal `exhausted`
                // flag, and the state stores the iterator directly).
                // Re-enable (`self.rng.chance(1, 4)`) once fixed.
                let inclusive = false;
                // Arguments are proptest-drawn from 0..16 and delegation
                // arguments are reduced mod 16, so an argument-bounded
                // range runs at most 15 times (16 inclusively).
                let iters = match (&upper, inclusive) {
                    (Upper::Lit(n), false) => *n as u64,
                    (Upper::Lit(n), true) => *n as u64 + 1,
                    (Upper::Var(_), false) => 15,
                    (Upper::Var(_), true) => 16,
                };
                self.push_var(&var, false, false);
                let body = self.gen_loop_body(iters, budget);
                self.scope.pop();
                Stmt::For {
                    var,
                    upper,
                    inclusive,
                    label,
                    body,
                }
            }
            1 => {
                let counter = self.fresh("c");
                let limit = 1 + self.rng.below(4) as u32;
                // The counter is declared just before the loop, so it
                // stays in scope for the rest of the enclosing block.
                self.push_var(&counter, false, false);
                let body = self.gen_loop_body(limit as u64, budget);
                Stmt::While {
                    counter,
                    limit,
                    label,
                    body,
                }
            }
            _ => {
                let counter = self.fresh("c");
                let limit = 1 + self.rng.below(4) as u32;
                self.push_var(&counter, false, false);
                let body = self.gen_loop_body(limit as u64, budget);
                Stmt::Loop {
                    counter,
                    limit,
                    label,
                    body,
                }
            }
        };
        self.loop_labels.pop();
        self.depth -= 1;
        stmt
    }

    fn gen_while_let(&mut self, budget: &mut i32) -> Stmt {
        let opt = self.pick_opt_var();
        let bind = self.fresh("p");
        let rebind = self.fresh("x");
        let limit = 1 + self.rng.below(4) as u32;
        let label = self.fresh_label();
        self.depth += 1;
        self.loop_labels.push(LoopLabel {
            id: label,
            value: false,
            block: false,
        });
        let mark = self.scope.len();
        self.push_var(&rebind, true, false);
        // The scrutinee strictly increases from at worst 0 up to `limit`
        // before going None, so the body runs at most limit + 1 times.
        let body = self.gen_loop_body(limit as u64 + 1, budget);
        self.scope.truncate(mark);
        self.loop_labels.pop();
        self.depth -= 1;
        Stmt::WhileLet {
            opt,
            bind,
            rebind,
            limit,
            label,
            body,
        }
    }

    fn gen_value_loop(&mut self, budget: &mut i32) -> Stmt {
        let counter = self.fresh("c");
        let limit = 1 + self.rng.below(4) as u32;
        let label = self.fresh_label();
        self.push_var(&counter, false, false);
        let fuel_value = self.gen_expr(1);
        // The loop is a value expression: hide enclosing labels and ban
        // `return`, but its own (value-carrying) label is jumpable.
        let prev_in_value = self.in_value;
        self.in_value = true;
        let saved_labels = std::mem::take(&mut self.loop_labels);
        self.loop_labels.push(LoopLabel {
            id: label,
            value: true,
            block: false,
        });
        self.depth += 1;
        let body = self.gen_loop_body(limit as u64, budget);
        self.depth -= 1;
        self.loop_labels = saved_labels;
        self.in_value = prev_in_value;
        let name = self.fresh("v");
        self.push_var(&name, true, false);
        Stmt::ValueLoop {
            name,
            counter,
            limit,
            label,
            fuel_value,
            body,
        }
    }

    fn gen_jump(&mut self) -> Option<Stmt> {
        if !self.loop_labels.is_empty() && (self.in_value || self.rng.chance(2, 3)) {
            // An unlabeled continue targets the nearest enclosing loop.
            // The innermost labeled construct must be a loop: crossing
            // a labeled block without naming a label is E0695.
            if self.loop_labels.last().is_some_and(|l| !l.block) && self.rng.chance(1, 5) {
                return Some(Stmt::ContinueBare);
            }
            let i = self.rng.below(self.loop_labels.len() as u64) as usize;
            let LoopLabel { id, value, block } = self.loop_labels[i];
            Some(if block || self.rng.chance(1, 2) {
                if value {
                    Stmt::BreakValue(id, self.gen_expr(2))
                } else {
                    Stmt::Break(id)
                }
            } else {
                Stmt::Continue(id)
            })
        } else if !self.in_value {
            Some(Stmt::Return(self.gen_ret_expr()))
        } else {
            None
        }
    }

    fn gen_ret_expr(&mut self) -> RetExpr {
        match self.flavor {
            Flavor::U32 => RetExpr::Plain(self.gen_expr(2)),
            Flavor::OptionU32 => {
                if self.rng.chance(1, 5) {
                    RetExpr::NoneLit
                } else if self.has_opt_var() && self.rng.chance(1, 4) {
                    RetExpr::OptVar(self.pick_opt_var())
                } else {
                    RetExpr::Wrapped(self.gen_expr(2))
                }
            }
            Flavor::ResultU32 => {
                if self.rng.chance(1, 5) {
                    RetExpr::ErrWrapped(self.gen_expr(2))
                } else {
                    RetExpr::OkWrapped(self.gen_expr(2))
                }
            }
        }
    }

    fn gen_delegate(&mut self) -> Stmt {
        let candidates: Vec<usize> = self
            .prior
            .iter()
            .enumerate()
            .filter(|(_, p)| self.delegation_fits(p.bound))
            .map(|(i, _)| i)
            .collect();
        let sub_case = *self.rng.pick(&candidates);
        let sub_flavor = self.prior[sub_case].flavor;
        self.bound += self.loop_mult * self.prior[sub_case].bound;
        // Reduced mod 16 to keep sub-case arguments in the same domain
        // as the proptest-generated ones: `for` loops bounded by an
        // argument (`Upper::Var`) are only fuel-bounded on that domain.
        let args = (
            Expr::Rem(Box::new(self.gen_expr(1)), 16),
            Expr::Rem(Box::new(self.gen_expr(1)), 16),
        );
        let sub_var = self.fresh("s");
        let bind = if self.rng.chance(1, 3) {
            DelegateBind::Discard
        } else {
            match sub_flavor {
                Flavor::U32 => {
                    let name = self.fresh("v");
                    self.push_var(&name, false, false);
                    DelegateBind::U32(name)
                }
                Flavor::OptionU32 => {
                    let name = self.fresh("o");
                    self.push_var(&name, false, true);
                    DelegateBind::Opt(name)
                }
                Flavor::ResultU32 => {
                    // The bound `Result<u32, Err1>` is a valid `?`
                    // operand too (reflexive `From<Err1> for Err1`).
                    let name = self.fresh("q");
                    self.push_res_var(&name);
                    DelegateBind::Res(name)
                }
            }
        };
        Stmt::Delegate {
            sub_case,
            sub_var,
            args,
            bind,
        }
    }

    fn gen_let_if_value(&mut self, budget: &mut i32) -> Stmt {
        let cond = self.gen_cond();
        let prev = self.in_value;
        self.in_value = true;
        // Hide enclosing loops so no jump can escape the value expression.
        let saved_labels = std::mem::take(&mut self.loop_labels);
        self.depth += 1;
        let (then_b, then_e) = self.gen_block_with_tail(budget);
        let (else_b, else_e) = self.gen_block_with_tail(budget);
        self.depth -= 1;
        self.loop_labels = saved_labels;
        self.in_value = prev;
        let name = self.fresh("v");
        self.push_var(&name, true, false);
        Stmt::LetIfValue {
            name,
            cond,
            then_b,
            then_e,
            else_b,
            else_e,
        }
    }

    fn gen_let_match_value(&mut self, budget: &mut i32) -> Stmt {
        let scrut = self.gen_expr(2);
        let modulus = 2 + self.rng.below(2) as u32;
        let prev = self.in_value;
        self.in_value = true;
        let saved_labels = std::mem::take(&mut self.loop_labels);
        self.depth += 1;
        let arms: Vec<(Option<Cond>, Vec<Stmt>, Expr)> = (0..modulus)
            .map(|j| {
                let guard = self.gen_arm_guard(j, modulus);
                let (b, e) = self.gen_block_with_tail(budget);
                (guard, b, e)
            })
            .collect();
        self.depth -= 1;
        self.loop_labels = saved_labels;
        self.in_value = prev;
        let name = self.fresh("v");
        self.push_var(&name, true, false);
        Stmt::LetMatchValue {
            name,
            scrut,
            modulus,
            arms,
        }
    }

    fn gen_expr(&mut self, depth: u32) -> Expr {
        if depth == 0 || self.rng.chance(1, 2) {
            if self.rng.chance(3, 5) {
                Expr::Var(self.pick_u32_var())
            } else {
                let v = self.rng.below(16) as u32;
                if self.rng.chance(1, 4) {
                    Expr::LitUn(v)
                } else {
                    Expr::Lit(v)
                }
            }
        } else {
            let has_arr = self.scope.iter().any(|v| v.arr);
            let n = if has_arr { 9 } else { 8 };
            match self.rng.below(n) {
                0 => Expr::WrapAdd(
                    Box::new(self.gen_expr(depth - 1)),
                    Box::new(self.gen_expr(depth - 1)),
                ),
                1 => Expr::WrapSub(
                    Box::new(self.gen_expr(depth - 1)),
                    Box::new(self.gen_expr(depth - 1)),
                ),
                2 => Expr::WrapMul(
                    Box::new(self.gen_expr(depth - 1)),
                    Box::new(self.gen_expr(depth - 1)),
                ),
                3 => Expr::Rem(
                    Box::new(self.gen_expr(depth - 1)),
                    2 + self.rng.below(5) as u32,
                ),
                4 => Expr::NegCast(Box::new(self.gen_expr(depth - 1))),
                5 => Expr::CastRound(Box::new(self.gen_expr(depth - 1))),
                6 => Expr::TupleField(
                    Box::new(self.gen_expr(depth - 1)),
                    Box::new(self.gen_expr(depth - 1)),
                    self.rng.below(2) as u8,
                ),
                7 => Expr::PairField {
                    x: Box::new(self.gen_expr(depth - 1)),
                    y: Box::new(self.gen_expr(depth - 1)),
                    second: self.rng.chance(1, 2),
                },
                _ => Expr::Index {
                    arr: self.pick_arr_var(),
                    idx: Box::new(self.gen_expr(depth - 1)),
                },
            }
        }
    }

    /// Pure guard for a literal match arm (never for the trailing `_`
    /// arm, which must stay unguarded for exhaustiveness). `Cond` is
    /// yield-free by construction, so guards stay pure.
    fn gen_arm_guard(&mut self, arm: u32, modulus: u32) -> Option<Cond> {
        if arm + 1 < modulus && self.rng.chance(2, 5) {
            Some(self.gen_cond())
        } else {
            None
        }
    }

    fn gen_cond(&mut self) -> Cond {
        // One level of `&&` / `||` composition; leaves stay pure, so
        // composed conditions are still valid match guards.
        if self.rng.chance(1, 5) {
            let a = Box::new(self.gen_cond_leaf());
            let b = Box::new(self.gen_cond_leaf());
            if self.rng.chance(1, 2) {
                Cond::And(a, b)
            } else {
                Cond::Or(a, b)
            }
        } else {
            self.gen_cond_leaf()
        }
    }

    fn gen_cond_leaf(&mut self) -> Cond {
        if self.rng.chance(1, 2) {
            Cond::Lt(self.gen_expr(1), self.gen_expr(1))
        } else {
            Cond::ModIsZero(self.gen_expr(1), 2 + self.rng.below(2) as u32)
        }
    }
}
