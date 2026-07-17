//! AST for the generated coroutine bodies. The shape mirrors the
//! syntactic subset documented in README "Constraints": every generated
//! program is supported-by-construction (annotated `let`s, pure loop
//! conditions, fuel-bounded loops, fresh names so no shadowing) and
//! panic-free (wrapping arithmetic, `%` by non-zero literals only).

pub enum Expr {
    Lit(u32),
    Var(String),
    WrapAdd(Box<Expr>, Box<Expr>),
    WrapSub(Box<Expr>, Box<Expr>),
    WrapMul(Box<Expr>, Box<Expr>),
    /// `expr % lit` with a non-zero literal modulus.
    Rem(Box<Expr>, u32),
}

pub enum Cond {
    Lt(Expr, Expr),
    ModIsZero(Expr, u32),
}

/// Upper bound of a `for` range: a literal or a function argument
/// (both syntactically known endpoints).
pub enum Upper {
    Lit(u32),
    Var(String),
}

pub enum Stmt {
    /// `let mut name: u32 = expr;`
    Let {
        name: String,
        expr: Expr,
    },
    /// `name = expr;`
    Assign {
        name: String,
        expr: Expr,
    },
    /// `yield_!(expr);`
    Yield(Expr),
    /// `let name = yield_!(arg);`
    LetYield {
        name: String,
        arg: Expr,
    },
    /// `let name: u32 = u32::wrapping_add(yield_!(a), yield_!(b));`
    /// — expression-position yields with a pure prefix.
    LetYieldAdd {
        name: String,
        a: Expr,
        b: Expr,
    },
    /// `name = u32::wrapping_add(name, yield_!(arg));`
    AssignYieldAdd {
        name: String,
        arg: Expr,
    },
    If {
        cond: Cond,
        then_b: Vec<Stmt>,
        else_b: Option<Vec<Stmt>>,
    },
    /// `match (scrut) % modulus { 0 => .., .., _ => .. }` with
    /// `modulus` arms (last one is `_`), literal patterns only.
    Match {
        scrut: Expr,
        modulus: u32,
        arms: Vec<Vec<Stmt>>,
    },
    /// `'lN: for var in 0u32..upper { .. }`
    For {
        var: String,
        upper: Upper,
        label: usize,
        body: Vec<Stmt>,
    },
    /// `let mut counter = 0; 'lN: while counter < limit { counter += 1; .. }`
    /// — the increment comes first so `continue` cannot skip it.
    While {
        counter: String,
        limit: u32,
        label: usize,
        body: Vec<Stmt>,
    },
    /// `let mut counter = 0; 'lN: loop { if counter >= limit { break; } counter += 1; .. }`
    Loop {
        counter: String,
        limit: u32,
        label: usize,
        body: Vec<Stmt>,
    },
    Break(usize),
    Continue(usize),
    Return(Expr),
    /// `let mut name: u32 = if cond { ..; then_e } else { ..; else_e };`
    LetIfValue {
        name: String,
        cond: Cond,
        then_b: Vec<Stmt>,
        then_e: Expr,
        else_b: Vec<Stmt>,
        else_e: Expr,
    },
    /// `let mut name: u32 = match (scrut) % modulus { .. };`
    LetMatchValue {
        name: String,
        scrut: Expr,
        modulus: u32,
        arms: Vec<(Vec<Stmt>, Expr)>,
    },
}

pub enum Tail {
    Expr(Expr),
    /// Trailing `yield_!(e)` — evaluates to the resume value.
    Yield(Expr),
}

pub struct Body {
    pub stmts: Vec<Stmt>,
    pub tail: Tail,
}
