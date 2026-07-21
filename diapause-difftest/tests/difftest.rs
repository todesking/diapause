//! Differential property tests over randomly generated coroutine
//! bodies, produced at build time by build.rs. Each case is one
//! coroutine compiled both through `#[diapause::coroutine]` and as a
//! plain reference function; proptest then explores arguments and
//! resume-value sequences (which shrink on failure).
//!
//! Reproduction: a failure report includes the full coroutine source;
//! the generated file records the seed
//! (`DIAPAUSE_DIFFTEST_SEED`/`DIAPAUSE_DIFFTEST_CASES` regenerate it).

include!(concat!(env!("OUT_DIR"), "/cases.rs"));
