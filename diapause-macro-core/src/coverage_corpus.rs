//! Coverage harness: expands every difftest-generated case through
//! `expand::expand` inside this crate's test binary, so that coverage
//! instrumentation (which cannot observe proc-macro execution inside
//! rustc) can measure which lowering paths the generated corpus hits.
//!
//! Corpus files are the `cases.rs` files rendered by
//! `diapause-difftest/build.rs`; each case module carries a `SOURCE`
//! string constant with the exact coroutine source. Set
//! `DIAPAUSE_MACRO_CORPUS` to a colon-separated list of such files;
//! without it, any `cases.rs` under the workspace `target/` build
//! output of diapause-difftest is used. The test skips (with a message)
//! when no corpus is found, so plain `cargo test` never fails for the
//! lack of one.

use std::fs;
use std::path::PathBuf;

/// Corpus files: explicit list from `DIAPAUSE_MACRO_CORPUS`, or every
/// difftest `cases.rs` found under the workspace target directory.
fn corpus_paths() -> Vec<PathBuf> {
    if let Ok(list) = std::env::var("DIAPAUSE_MACRO_CORPUS") {
        return list.split(':').map(PathBuf::from).collect();
    }
    let build_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/debug/build");
    let Ok(entries) = fs::read_dir(&build_dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("diapause-difftest-")
        })
        .map(|e| e.path().join("out/cases.rs"))
        .filter(|p| p.is_file())
        .collect();
    paths.sort();
    paths
}

/// Pulls every `SOURCE` raw-string constant out of a rendered cases.rs.
fn extract_sources(text: &str) -> Vec<String> {
    const OPEN: &str = "SOURCE: &str = r##\"";
    const CLOSE: &str = "\"##";
    let mut sources = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        rest = &rest[start + OPEN.len()..];
        let end = rest
            .find(CLOSE)
            .expect("unterminated SOURCE raw string in corpus file");
        sources.push(rest[..end].to_string());
        rest = &rest[end + CLOSE.len()..];
    }
    sources
}

/// Expands one case source exactly the way rustc would for the
/// difftest crate: same attribute arguments, same extra derives.
fn expand_case(source: &str) -> Result<(), String> {
    let mut item: syn::ItemFn =
        syn::parse_str(source).map_err(|e| format!("source does not parse: {e}"))?;
    let idx = item
        .attrs
        .iter()
        .position(|a| {
            a.path()
                .segments
                .last()
                .is_some_and(|s| s.ident == "coroutine")
        })
        .ok_or("no #[diapause::coroutine] attribute in SOURCE")?;
    let attr = item.attrs.remove(idx);
    let args = match attr.meta {
        syn::Meta::List(list) => list.tokens,
        _ => proc_macro2::TokenStream::new(),
    };
    // The difftest render places these derives below the attribute, so
    // they arrive in `item.attrs` of the real expansion too.
    item.attrs
        .push(syn::parse_quote!(#[derive(Clone, serde::Serialize, serde::Deserialize)]));
    let out = crate::expand::expand(args, item).map_err(|e| format!("expansion failed: {e}"))?;
    syn::parse2::<syn::File>(out)
        .map(|_| ())
        .map_err(|e| format!("expansion output does not parse: {e}"))
}

#[test]
fn expand_difftest_corpus() {
    let paths = corpus_paths();
    if paths.is_empty() {
        eprintln!(
            "coverage_corpus: no corpus found (set DIAPAUSE_MACRO_CORPUS or build \
             diapause-difftest first); skipping"
        );
        return;
    }
    let mut total = 0usize;
    let mut failures = Vec::new();
    for path in &paths {
        let text = fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("cannot read corpus {}: {e}", path.display()));
        let sources = extract_sources(&text);
        assert!(
            !sources.is_empty(),
            "no SOURCE constants in corpus {}",
            path.display()
        );
        for (i, src) in sources.iter().enumerate() {
            total += 1;
            if let Err(msg) = expand_case(src) {
                failures.push(format!("--- {} case {i}: {msg}\n{src}\n", path.display()));
            }
        }
    }
    eprintln!(
        "coverage_corpus: expanded {total} cases from {} corpus file(s)",
        paths.len()
    );
    assert!(
        failures.is_empty(),
        "{} of {total} cases failed to expand:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
