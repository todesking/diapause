//! AST for the generated coroutine bodies. The shape mirrors the
//! syntactic subset documented in README "Constraints": every generated
//! program is supported-by-construction (annotated `let`s, pure loop
//! conditions, fuel-bounded loops, fresh names so no shadowing) and
//! panic-free (wrapping arithmetic, `%` by non-zero literals only).

/// The coroutine's return type. `OptionU32` bodies may use the `?`
/// operator and `None`-returning paths. `ResultU32` bodies
/// (`Result<u32, Err1>`) apply `?` to `Result<u32, Err2>` values,
/// exercising From-based error conversion.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    U32,
    OptionU32,
    ResultU32,
}

/// A whole test case: one coroutine body plus its per-case knobs.
pub struct Case {
    pub body: Body,
    pub flavor: Flavor,
    pub fingerprint: bool,
    pub has_delegate: bool,
}

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

/// A value of the coroutine's return type, for `return` and the tail.
pub enum RetExpr {
    /// `expr` (U32 flavor).
    Plain(Expr),
    /// `Some(expr)` (OptionU32 flavor).
    Wrapped(Expr),
    /// `None` (OptionU32 flavor).
    NoneLit,
    /// An `Option<u32>` variable (OptionU32 flavor).
    OptVar(String),
    /// `Ok(expr)` (ResultU32 flavor).
    OkWrapped(Expr),
    /// `Err(Err1(expr))` (ResultU32 flavor).
    ErrWrapped(Expr),
}

/// What a delegation binds its completion value to.
pub enum DelegateBind {
    /// `yield_all!(sub);` — value discarded.
    Discard,
    /// `let name: u32 = yield_all!(sub);`
    U32(String),
    /// `let mut name: Option<u32> = yield_all!(sub);`
    Opt(String),
    /// `let name: Result<u32, Err1> = yield_all!(sub);`
    Res(String),
}

pub enum Stmt {
    /// `let mut name: u32 = expr;`
    Let {
        name: String,
        expr: Expr,
    },
    /// `let mut name: Option<u32> = Some(expr) / None;`
    LetOption {
        name: String,
        init: Option<Expr>,
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
    /// `let name: u32 = opt?;` (OptionU32 flavor only).
    LetTry {
        name: String,
        opt: String,
    },
    /// `let name: Result<u32, Err2> = Ok(expr) / Err(Err2(expr));`
    /// (ResultU32 flavor only).
    LetResult {
        name: String,
        init: Result<Expr, Expr>,
    },
    /// `let name: u32 = res?;` (ResultU32 flavor only) — the error
    /// side goes through `From`-based conversion.
    LetTryResult {
        name: String,
        res: String,
    },
    If {
        cond: Cond,
        then_b: Vec<Stmt>,
        else_b: Option<Vec<Stmt>>,
    },
    /// `if let Some(bind) = opt { let mut rebind: u32 = bind; .. }` —
    /// the pattern binding is rebound immediately so it never crosses
    /// a yield.
    IfLet {
        opt: String,
        bind: String,
        rebind: String,
        then_b: Vec<Stmt>,
        else_b: Option<Vec<Stmt>>,
    },
    /// `match (scrut) % modulus { 0 => .., .., _ => .. }` with
    /// `modulus` arms (last one is `_`), literal patterns only.
    /// Literal arms may carry a pure (yield-free) guard; a false guard
    /// falls through to the `_` arm.
    Match {
        scrut: Expr,
        modulus: u32,
        arms: Vec<(Option<Cond>, Vec<Stmt>)>,
    },
    /// `let 0u32 = (scrut) % modulus else { ..; <jump> };` — the else
    /// block always ends in a diverging jump, and the pattern has no
    /// bindings.
    LetElse {
        scrut: Expr,
        modulus: u32,
        body: Vec<Stmt>,
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
    /// `'lN: while let Some(bind) = opt { let mut rebind = bind;
    /// opt = if rebind < limit { Some(rebind + 1) } else { None }; .. }`
    /// — the scrutinee strictly increases toward `limit` before the
    /// body runs, so every run terminates.
    WhileLet {
        opt: String,
        bind: String,
        rebind: String,
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
    /// `let mut counter = 0; let mut name: u32 = 'lN: loop {
    /// if counter >= limit { break 'lN fuel_value; } counter += 1; .. };`
    /// — a value-producing loop; every break to its label carries a
    /// value.
    ValueLoop {
        name: String,
        counter: String,
        limit: u32,
        label: usize,
        fuel_value: Expr,
        body: Vec<Stmt>,
    },
    Break(usize),
    /// `break 'lN expr;` — targets a `ValueLoop` label.
    BreakValue(usize, Expr),
    Continue(usize),
    Return(RetExpr),
    /// `let mut name: u32 = if cond { ..; then_e } else { ..; else_e };`
    LetIfValue {
        name: String,
        cond: Cond,
        then_b: Vec<Stmt>,
        then_e: Expr,
        else_b: Vec<Stmt>,
        else_e: Expr,
    },
    /// `let mut name: u32 = match (scrut) % modulus { .. };` — literal
    /// arms may carry a pure guard, as in `Match`.
    LetMatchValue {
        name: String,
        scrut: Expr,
        modulus: u32,
        arms: Vec<(Option<Cond>, Vec<Stmt>, Expr)>,
    },
    /// `yield_all!` delegation to an earlier generated case. This is
    /// the one construct rendered differently per world: the reference
    /// side calls the sub-case's reference function directly (the
    /// sub-coroutine's state type does not exist in the plain-function
    /// world).
    Delegate {
        sub_case: usize,
        sub_var: String,
        args: (Expr, Expr),
        bind: DelegateBind,
    },
}

pub enum Tail {
    Ret(RetExpr),
    /// Trailing `yield_!(e)` — evaluates to the resume value.
    Yield(Expr),
    /// Trailing `Some(yield_!(e))` (OptionU32 flavor).
    YieldWrapped(Expr),
    /// Trailing `Ok(yield_!(e))` (ResultU32 flavor).
    YieldOk(Expr),
}

pub struct Body {
    pub stmts: Vec<Stmt>,
    pub tail: Tail,
}
