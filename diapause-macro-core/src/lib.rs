//! Implementation of the `#[diapause::coroutine]` transformation.
//!
//! This crate contains the whole macro pipeline (parsing, lowering,
//! analysis, and code generation) as an ordinary library so it can be
//! built for non-proc-macro targets such as wasm. The `diapause-macro`
//! crate is a thin proc-macro shim over [`expand`]; users should depend
//! on `diapause`, which re-exports the attribute.

// `cfg` and `analyze_cfg` are public so that debugging front ends (the
// playground) can inspect the intermediate artifacts `expand_debug`
// returns; `expand` alone is enough for macro expansion itself.
pub mod analyze_cfg;
mod args;
pub mod cfg;
#[cfg(test)]
mod coverage_corpus;
mod expand;
mod hoist;
mod lower;
mod signature;
#[cfg(test)]
mod test_util;
mod ty_infer;
mod validate;

pub use expand::{DebugExpansion, expand, expand_debug};
