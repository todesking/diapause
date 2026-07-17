//! Random generation of supported coroutine bodies.
//!
//! Validity is guaranteed by construction:
//! - every `let` is annotated `u32` (syntactic-type rule), names are
//!   globally fresh (no shadowing, so opaque jumps are always legal);
//! - loops are fuel-bounded, so every body terminates on any input;
//! - a conditional gets at most one branch ending in a jump, so the
//!   trailing expression is always statically reachable;
//! - inside value-position `if`/`match` no jumps are generated at all,
//!   and enclosing loop labels are hidden from nested generation;
//! - all arithmetic is wrapping and `%` uses non-zero literals, so no
//!   generated program can panic.

use crate::ast::*;
use crate::rng::Rng;

pub fn generate(seed: u64, index: usize) -> Body {
    let case_seed = seed
        ^ (index as u64)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(0x5EED);
    Gen::new(Rng::new(case_seed)).gen_body()
}

struct Var {
    name: String,
    /// Only `let mut` bindings may be assignment targets; loop
    /// variables and counters are read-only to preserve termination.
    assignable: bool,
}

struct Gen {
    rng: Rng,
    scope: Vec<Var>,
    loop_labels: Vec<usize>,
    next_var: usize,
    next_label: usize,
    depth: usize,
    yields: usize,
    in_value: bool,
}

impl Gen {
    fn new(rng: Rng) -> Self {
        Gen {
            rng,
            scope: vec![
                Var {
                    name: "a0".into(),
                    assignable: false,
                },
                Var {
                    name: "a1".into(),
                    assignable: false,
                },
            ],
            loop_labels: Vec::new(),
            next_var: 0,
            next_label: 0,
            depth: 0,
            yields: 0,
            in_value: false,
        }
    }

    fn gen_body(&mut self) -> Body {
        let mut budget = 4 + self.rng.below(10) as i32;
        let stmts = self.gen_block(&mut budget);
        let tail = if self.yields == 0 || self.rng.chance(3, 10) {
            self.yields += 1;
            Tail::Yield(self.gen_expr(2))
        } else {
            Tail::Expr(self.gen_expr(2))
        };
        Body { stmts, tail }
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

    fn gen_stmt(&mut self, budget: &mut i32) -> Stmt {
        *budget -= 1;
        let structural = self.depth < 4 && *budget > 0;
        let has_assignable = self.scope.iter().any(|v| v.assignable);
        let mut kinds: Vec<(u64, u8)> = vec![(3, 0), (3, 1), (2, 2), (1, 3)];
        if has_assignable {
            kinds.push((2, 4));
            kinds.push((1, 5));
        }
        if structural {
            kinds.push((3, 6));
            kinds.push((2, 7));
            kinds.push((3, 8));
            kinds.push((1, 9));
            kinds.push((1, 10));
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
                self.scope.push(Var {
                    name: name.clone(),
                    assignable: true,
                });
                Stmt::Let { name, expr }
            }
            1 => {
                self.yields += 1;
                Stmt::Yield(self.gen_expr(2))
            }
            2 => {
                self.yields += 1;
                let arg = self.gen_expr(2);
                let name = self.fresh("r");
                self.scope.push(Var {
                    name: name.clone(),
                    assignable: false,
                });
                Stmt::LetYield { name, arg }
            }
            3 => {
                self.yields += 2;
                let a = self.gen_expr(1);
                let b = self.gen_expr(1);
                let name = self.fresh("r");
                self.scope.push(Var {
                    name: name.clone(),
                    assignable: false,
                });
                Stmt::LetYieldAdd { name, a, b }
            }
            4 => Stmt::Assign {
                name: self.pick_assignable(),
                expr: self.gen_expr(2),
            },
            5 => {
                self.yields += 1;
                Stmt::AssignYieldAdd {
                    name: self.pick_assignable(),
                    arg: self.gen_expr(1),
                }
            }
            6 => self.gen_if(budget),
            7 => self.gen_match(budget),
            8 => self.gen_loop(budget),
            9 => self.gen_let_if_value(budget),
            _ => self.gen_let_match_value(budget),
        }
    }

    fn pick_assignable(&mut self) -> String {
        let names: Vec<&str> = self
            .scope
            .iter()
            .filter(|v| v.assignable)
            .map(|v| v.name.as_str())
            .collect();
        (*self.rng.pick(&names)).to_string()
    }

    fn gen_if(&mut self, budget: &mut i32) -> Stmt {
        let cond = self.gen_cond();
        self.depth += 1;
        let mut then_b = self.gen_block(budget);
        let mut else_b = if self.rng.chance(1, 2) {
            Some(self.gen_block(budget))
        } else {
            None
        };
        self.depth -= 1;
        if !self.in_value && self.rng.chance(1, 4) {
            let jump = self.gen_jump();
            match (&mut else_b, self.rng.chance(1, 2)) {
                (Some(b), true) => b.push(jump),
                _ => then_b.push(jump),
            }
        }
        Stmt::If {
            cond,
            then_b,
            else_b,
        }
    }

    fn gen_match(&mut self, budget: &mut i32) -> Stmt {
        let scrut = self.gen_expr(2);
        let modulus = 2 + self.rng.below(2) as u32;
        self.depth += 1;
        let mut arms: Vec<Vec<Stmt>> = (0..modulus).map(|_| self.gen_block(budget)).collect();
        self.depth -= 1;
        if !self.in_value && self.rng.chance(1, 4) {
            let jump = self.gen_jump();
            let i = self.rng.below(modulus as u64) as usize;
            arms[i].push(jump);
        }
        Stmt::Match {
            scrut,
            modulus,
            arms,
        }
    }

    fn gen_loop(&mut self, budget: &mut i32) -> Stmt {
        let label = self.fresh_label();
        self.depth += 1;
        self.loop_labels.push(label);
        let stmt = match self.rng.below(3) {
            0 => {
                let var = self.fresh("i");
                let upper = if self.rng.chance(2, 5) {
                    Upper::Var(if self.rng.chance(1, 2) { "a0" } else { "a1" }.to_string())
                } else {
                    Upper::Lit(1 + self.rng.below(4) as u32)
                };
                self.scope.push(Var {
                    name: var.clone(),
                    assignable: false,
                });
                let body = self.gen_block(budget);
                self.scope.pop();
                Stmt::For {
                    var,
                    upper,
                    label,
                    body,
                }
            }
            1 => {
                let counter = self.fresh("c");
                let limit = 1 + self.rng.below(4) as u32;
                // The counter is declared just before the loop, so it
                // stays in scope for the rest of the enclosing block.
                self.scope.push(Var {
                    name: counter.clone(),
                    assignable: false,
                });
                let body = self.gen_block(budget);
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
                self.scope.push(Var {
                    name: counter.clone(),
                    assignable: false,
                });
                let body = self.gen_block(budget);
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

    fn gen_jump(&mut self) -> Stmt {
        if !self.loop_labels.is_empty() && self.rng.chance(2, 3) {
            let l = *self.rng.pick(&self.loop_labels);
            if self.rng.chance(1, 2) {
                Stmt::Break(l)
            } else {
                Stmt::Continue(l)
            }
        } else {
            Stmt::Return(self.gen_expr(2))
        }
    }

    fn gen_let_if_value(&mut self, budget: &mut i32) -> Stmt {
        let cond = self.gen_cond();
        let prev = self.in_value;
        self.in_value = true;
        let saved_labels = std::mem::take(&mut self.loop_labels);
        self.depth += 1;
        let (then_b, then_e) = self.gen_block_with_tail(budget);
        let (else_b, else_e) = self.gen_block_with_tail(budget);
        self.depth -= 1;
        self.loop_labels = saved_labels;
        self.in_value = prev;
        let name = self.fresh("v");
        self.scope.push(Var {
            name: name.clone(),
            assignable: true,
        });
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
        let arms: Vec<(Vec<Stmt>, Expr)> = (0..modulus)
            .map(|_| self.gen_block_with_tail(budget))
            .collect();
        self.depth -= 1;
        self.loop_labels = saved_labels;
        self.in_value = prev;
        let name = self.fresh("v");
        self.scope.push(Var {
            name: name.clone(),
            assignable: true,
        });
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
                let names: Vec<&str> = self.scope.iter().map(|v| v.name.as_str()).collect();
                Expr::Var((*self.rng.pick(&names)).to_string())
            } else {
                Expr::Lit(self.rng.below(16) as u32)
            }
        } else {
            match self.rng.below(4) {
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
                _ => Expr::Rem(
                    Box::new(self.gen_expr(depth - 1)),
                    2 + self.rng.below(5) as u32,
                ),
            }
        }
    }

    fn gen_cond(&mut self) -> Cond {
        if self.rng.chance(1, 2) {
            Cond::Lt(self.gen_expr(1), self.gen_expr(1))
        } else {
            Cond::ModIsZero(self.gen_expr(1), 2 + self.rng.below(2) as u32)
        }
    }
}
