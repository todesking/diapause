//! wasm entry point for the baregen playground.
//!
//! Compiled to `wasm32-unknown-unknown` with wasm-bindgen, this crate
//! exposes [`transform`](transform_js) to JavaScript: it takes a pasted
//! function with its `#[baregen::coroutine(...)]` attribute and returns
//! the expansion, CFG DOT renderings, and positioned errors as a plain
//! JS object. The logic lives in [`transform`] (the module) so it can
//! be tested on the host target.

pub mod cfg_dot;
mod transform;

pub use transform::{ErrorInfo, TransformOutput, transform};

use wasm_bindgen::prelude::*;

/// Returns `{ expanded: string, cfg_dot_raw: string|null,
/// cfg_dot_simplified: string|null, errors: [{message, line, col,
/// end_line, end_col}] }`.
///
/// Serialization goes through a JSON string and `JSON.parse` so that
/// absent DOT outputs surface as `null` (not `undefined`).
#[wasm_bindgen(js_name = transform)]
pub fn transform_js(source: &str) -> JsValue {
    let report = transform(source);
    let json = serde_json::to_string(&report).expect("TransformOutput always serializes");
    js_sys::JSON::parse(&json).expect("serde_json output is valid JSON")
}
