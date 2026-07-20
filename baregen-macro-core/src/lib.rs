//! Implementation of the `#[baregen::coroutine]` transformation.
//!
//! This crate contains the whole macro pipeline (parsing, lowering,
//! analysis, and code generation) as an ordinary library so it can be
//! built for non-proc-macro targets such as wasm. The `baregen-macro`
//! crate is a thin proc-macro shim over [`expand`]; users should depend
//! on `baregen`, which re-exports the attribute.

mod analyze_cfg;
mod args;
mod cfg;
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

pub use expand::expand;
