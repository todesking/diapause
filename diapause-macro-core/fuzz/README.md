# diapause-macro-core fuzzing

Fuzz target for `diapause_macro_core::expand`. It reads the input bytes
as UTF-8 source of a single `fn`, parses it into a `syn::ItemFn`,
extracts the `#[diapause::coroutine(..)]` attribute arguments (if
present), attaches the standard derives, and calls `expand`. The
invariant under test: **any input must fail as a `syn::Error`
diagnostic (which the real proc-macro turns into a compile error), never
as a panic.** Parse and expansion errors are expected and ignored.

This crate is deliberately excluded from the workspace (see the root
`Cargo.toml`) because `cargo-fuzz` needs the nightly toolchain and
libfuzzer runtime, which must not affect the stable workspace build.

## Running

```sh
# from this directory
cargo +nightly fuzz run expand corpus/expand seed_corpus/expand \
    -- -max_total_time=300 -max_len=4096 -timeout=10
```

`seed_corpus/expand` holds committed seeds: the coroutine sources from
the difftest-generated `cases.rs` corpus plus a few hand-authored ones.
The live `corpus/`, `artifacts/`, and `target/` directories are
git-ignored.

## Reproducing a crash

```sh
cargo +nightly fuzz run expand artifacts/expand/crash-<hash>
cargo +nightly fuzz tmin expand artifacts/expand/crash-<hash>   # minimize
```

Decode the raw input bytes to source with:

```sh
python3 -c "import sys;sys.stdout.write(open('artifacts/expand/crash-<hash>','rb').read().decode('utf-8','replace'))"
```

Every panic found here should get a minimal regression test in
`diapause-macro-core` before the fix.
