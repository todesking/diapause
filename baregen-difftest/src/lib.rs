//! Differential-testing harness for baregen.
//!
//! The same coroutine body text is compiled twice: once through
//! `#[baregen::coroutine]` into a state machine, and once as a plain
//! function in which `yield_!` resolves to this crate's [`yield_`]
//! macro, which records the yielded value and returns a scripted resume
//! value. The plain execution is the semantics oracle; [`check_case`]
//! compares the two traces and additionally serde-round-trips and
//! clones the state machine at every suspension point.
//!
//! The random coroutine bodies live in `tests/difftest.rs`, generated
//! at build time by `build.rs` (see `build/`).

use baregen::{Coroutine, CoroutineState};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The yield values and completion value of one full run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    pub yields: Vec<u32>,
    pub complete: u32,
}

/// Generated bodies are terminating by construction, so exceeding this
/// many suspensions in one run means a generator bug.
const MAX_YIELDS: usize = 10_000;

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

    /// Runs a reference body under the given resume script and captures
    /// its trace. If `f` panics the stale script is simply overwritten
    /// by the next run on this thread.
    pub fn run_reference(f: impl FnOnce() -> u32, resumes: &[u32]) -> super::Trace {
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
/// version compiles in both worlds (the `#[baregen::coroutine]` version
/// consumes its `yield_!` tokens before name resolution).
#[macro_export]
macro_rules! yield_ {
    ($e:expr) => {
        $crate::oracle::yield_value($e)
    };
}

fn resume_at(resumes: &[u32], i: usize) -> u32 {
    resumes[i % resumes.len()]
}

/// Drives the state machine to completion, feeding the cycled resume
/// script exactly as the oracle does.
pub fn drive_plain<C>(mut c: C, resumes: &[u32]) -> Result<Trace, String>
where
    C: Coroutine<u32, Yield = u32, Return = u32>,
{
    let mut yields = Vec::new();
    let mut step = c.start();
    loop {
        match step {
            CoroutineState::Yielded(v) => {
                if yields.len() >= MAX_YIELDS {
                    return Err(format!("state machine exceeded {MAX_YIELDS} yields"));
                }
                yields.push(v);
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
pub fn drive_tortured<C>(mut c: C, resumes: &[u32]) -> Result<Trace, String>
where
    C: Coroutine<u32, Yield = u32, Return = u32> + Clone + Serialize + DeserializeOwned,
{
    let mut yields = Vec::new();
    let mut step = c.start();
    loop {
        match step {
            CoroutineState::Yielded(v) => {
                if yields.len() >= MAX_YIELDS {
                    return Err(format!("state machine exceeded {MAX_YIELDS} yields"));
                }
                yields.push(v);
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
    reference: impl FnOnce() -> u32,
    machine: C,
) where
    C: Coroutine<u32, Yield = u32, Return = u32> + Clone + Serialize + DeserializeOwned,
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
        "baregen difftest: {kind}\nargs: {args:?}\nresumes: {resumes:?}\n{detail}\n\
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
