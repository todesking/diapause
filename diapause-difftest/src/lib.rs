//! Differential-testing harness for diapause.
//!
//! The same coroutine body text is compiled twice: once through
//! `#[diapause::coroutine]` into a state machine, and once as a plain
//! function in which `yield_!` resolves to this crate's [`yield_`]
//! macro, which records the yielded value and returns a scripted resume
//! value. The plain execution is the semantics oracle; [`check_case`]
//! compares the two traces and additionally serde-round-trips and
//! clones the state machine at every suspension point.
//!
//! The random coroutine bodies live in `tests/difftest.rs`, generated
//! at build time by `build.rs` (see `build/`).

use diapause::{Coroutine, CoroutineState};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The yield values and completion value of one full run. `R` is the
/// coroutine's return type (`u32`, or `Option<u32>` for bodies that
/// exercise the `?` operator).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace<R> {
    pub yields: Vec<u32>,
    pub complete: R,
}

/// Generated bodies are terminating by construction, so exceeding this
/// many suspensions in one run means a generator bug.
const MAX_YIELDS: usize = 10_000;

/// How a yield value is recorded in a [`Trace`]. `()` yields (the
/// argument-less `yield_!()` form, coroutines with `yield = ()`) carry
/// no information, so recording them as a constant loses nothing while
/// keeping yield count and resume scheduling under test.
pub trait YieldRepr: Copy {
    fn repr(self) -> u32;
}

impl YieldRepr for u32 {
    fn repr(self) -> u32 {
        self
    }
}

impl YieldRepr for () {
    fn repr(self) -> u32 {
        0
    }
}

/// Error types for the `Result` flavor: generated bodies apply `?` to
/// `Result<u32, Err2>` values inside coroutines returning
/// `Result<u32, Err1>`, exercising the From-based error conversion of
/// `FromResidual`. `Copy` so generated `?` operands can be reused
/// freely, like the `Option<u32>` variables.
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Debug)]
pub struct Err1(pub u32);

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Debug)]
pub struct Err2(pub u32);

/// Fixed struct for generated struct-literal + field-access expressions
/// (`diapause_difftest::Pair { x: .., y: .. }.x`). `Copy` like every
/// other generated value type; instances only appear inside pure
/// subexpressions and never cross a suspension.
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Debug)]
pub struct Pair {
    pub x: u32,
    pub y: u32,
}

impl From<Err2> for Err1 {
    fn from(e: Err2) -> Self {
        Err1(e.0)
    }
}

pub mod oracle {
    //! Reference-execution side: `yield_!` in a reference function calls
    //! [`yield_value`], so the plain function's own control flow decides
    //! what gets yielded and when.

    use std::cell::RefCell;

    struct Script {
        resumes: Vec<u32>,
        idx: usize,
        yields: Vec<u32>,
    }

    thread_local! {
        static SCRIPT: RefCell<Option<Script>> = const { RefCell::new(None) };
    }

    /// Records `v` as yielded and returns the next scripted resume value
    /// (the resume list is cycled, so it never runs out).
    pub fn yield_value(v: u32) -> u32 {
        SCRIPT.with(|s| {
            let mut s = s.borrow_mut();
            let script = s
                .as_mut()
                .expect("yield_! called outside oracle::run_reference");
            assert!(
                script.yields.len() < super::MAX_YIELDS,
                "reference run exceeded {} yields — non-terminating generated body?",
                super::MAX_YIELDS
            );
            script.yields.push(v);
            let r = script.resumes[script.idx % script.resumes.len()];
            script.idx += 1;
            r
        })
    }

    /// `yield_!()` in a reference body: records the unit yield (as 0,
    /// see [`crate::YieldRepr`]) and returns the scripted resume value.
    pub fn yield_value_unit() -> u32 {
        yield_value(0)
    }

    /// Runs a reference body under the given resume script and captures
    /// its trace. If `f` panics the stale script is simply overwritten
    /// by the next run on this thread.
    pub fn run_reference<R>(f: impl FnOnce() -> R, resumes: &[u32]) -> super::Trace<R> {
        assert!(!resumes.is_empty(), "resume script must be non-empty");
        SCRIPT.with(|s| {
            *s.borrow_mut() = Some(Script {
                resumes: resumes.to_vec(),
                idx: 0,
                yields: Vec::new(),
            });
        });
        let complete = f();
        let yields = SCRIPT.with(|s| s.borrow_mut().take().unwrap().yields);
        super::Trace { yields, complete }
    }
}

/// `yield_!` for reference functions. Import this into the module that
/// holds the reference body so the body text shared with the coroutine
/// version compiles in both worlds (the `#[diapause::coroutine]` version
/// consumes its `yield_!` tokens before name resolution).
#[macro_export]
macro_rules! yield_ {
    () => {
        $crate::oracle::yield_value_unit()
    };
    ($e:expr) => {
        $crate::oracle::yield_value($e)
    };
}

fn resume_at(resumes: &[u32], i: usize) -> u32 {
    resumes[i % resumes.len()]
}

/// Drives the state machine to completion, feeding the cycled resume
/// script exactly as the oracle does.
pub fn drive_plain<C>(mut c: C, resumes: &[u32]) -> Result<Trace<C::Return>, String>
where
    C: Coroutine<u32>,
    C::Yield: YieldRepr,
{
    let mut yields = Vec::new();
    let mut step = c.start();
    loop {
        match step {
            CoroutineState::Yielded(v) => {
                if yields.len() >= MAX_YIELDS {
                    return Err(format!("state machine exceeded {MAX_YIELDS} yields"));
                }
                yields.push(v.repr());
                step = c.resume(resume_at(resumes, yields.len() - 1));
            }
            CoroutineState::Complete(v) => {
                return Ok(Trace {
                    yields,
                    complete: v,
                });
            }
        }
    }
}

/// Like [`drive_plain`], but at every suspension point the state is
/// round-tripped through serde_json and then cloned; the round-tripped
/// state and its clone are resumed with the same value and must take
/// the same step. The run continues on the clone, so both the serde and
/// the `Clone` path stay under test for the whole run.
pub fn drive_tortured<C>(mut c: C, resumes: &[u32]) -> Result<Trace<C::Return>, String>
where
    C: Coroutine<u32> + Clone + Serialize + DeserializeOwned,
    C::Yield: YieldRepr + PartialEq + core::fmt::Debug,
    C::Return: PartialEq + core::fmt::Debug,
{
    let mut yields = Vec::new();
    let mut step = c.start();
    loop {
        match step {
            CoroutineState::Yielded(v) => {
                if yields.len() >= MAX_YIELDS {
                    return Err(format!("state machine exceeded {MAX_YIELDS} yields"));
                }
                yields.push(v.repr());
                let i = yields.len() - 1;
                let json = serde_json::to_string(&c)
                    .map_err(|e| format!("serialize failed at suspension {i}: {e}"))?;
                let mut restored: C = serde_json::from_str(&json)
                    .map_err(|e| format!("deserialize failed at suspension {i}: {e}"))?;
                let mut snapshot = restored.clone();
                let r = resume_at(resumes, i);
                let restored_step = restored.resume(r);
                let snapshot_step = snapshot.resume(r);
                if restored_step != snapshot_step {
                    return Err(format!(
                        "clone diverged at suspension {i}: round-tripped state stepped to \
                         {restored_step:?}, its clone to {snapshot_step:?}"
                    ));
                }
                c = snapshot;
                step = snapshot_step;
            }
            CoroutineState::Complete(v) => {
                return Ok(Trace {
                    yields,
                    complete: v,
                });
            }
        }
    }
}

/// Entry point called by the generated tests: checks that the state
/// machine (plain and tortured) produces the reference trace.
pub fn check_case<C>(
    source: &str,
    args: &[u32],
    resumes: &[u32],
    reference: impl FnOnce() -> C::Return,
    machine: C,
) where
    C: Coroutine<u32> + Clone + Serialize + DeserializeOwned,
    C::Yield: YieldRepr + PartialEq + core::fmt::Debug,
    C::Return: PartialEq + core::fmt::Debug,
{
    let expected = oracle::run_reference(reference, resumes);
    match drive_plain(machine.clone(), resumes) {
        Ok(actual) if actual == expected => {}
        Ok(actual) => fail(
            source,
            args,
            resumes,
            "plain run diverged from reference",
            &format!("reference: {expected:?}\nstate machine: {actual:?}"),
        ),
        Err(e) => fail(source, args, resumes, "plain run failed", &e),
    }
    match drive_tortured(machine, resumes) {
        Ok(actual) if actual == expected => {}
        Ok(actual) => fail(
            source,
            args,
            resumes,
            "serde round-trip + clone run diverged from reference",
            &format!("reference: {expected:?}\nstate machine: {actual:?}"),
        ),
        Err(e) => fail(
            source,
            args,
            resumes,
            "serde round-trip + clone run failed",
            &e,
        ),
    }
}

fn fail(source: &str, args: &[u32], resumes: &[u32], kind: &str, detail: &str) -> ! {
    panic!(
        "diapause difftest: {kind}\nargs: {args:?}\nresumes: {resumes:?}\n{detail}\n\
         coroutine under test:\n{source}\n"
    );
}

/// Proptest config for the generated tests: regression-file persistence
/// is disabled because the generated cases change with the generator
/// seed, so persisted regressions would misapply. Case count etc. stay
/// env-configurable (`PROPTEST_CASES`).
pub fn proptest_config() -> proptest::test_runner::Config {
    proptest::test_runner::Config {
        failure_persistence: None,
        ..Default::default()
    }
}
