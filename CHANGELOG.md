# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The `diapause`, `diapause-macro`, and `diapause-macro-core` crates are
versioned and released together; this changelog covers all three.

## [Unreleased]

## [0.1.0] - Unreleased

Initial release.

### Added

- `#[diapause::coroutine]` attribute that rewrites a function into a
  state machine enum implementing the `Coroutine` trait, on stable Rust
  with no `async`, no `Pin`, no allocation, and no unsafe code.
- `yield_!` suspension points inside `if` / `if let` / `match` / `loop` /
  `while` / `while let` / `for` and the diverging block of `let ... else`,
  at any nesting depth, combined with `break` / `continue` (including
  labeled forms), early `return`, and the `?` operator on `Result` and
  `Option`.
- Expression-position `yield_!` with a pure prefix (hoisted into its own
  `let`), and value-producing yield-containing control flow as a `let`
  initializer or the function's trailing expression.
- `yield_all!` delegation to another coroutine (the analogue of Python's
  `yield from`), with the nested state stored by value so `Clone` and
  serde derives compose.
- `Coroutine` trait with `start` / `resume` / `status` / `is_started` /
  `is_done` and non-panicking `try_start` / `try_resume`, plus a
  forwarding impl for `&mut C`.
- `CoroutineState` and `CoroutineStatus` enums, including `Poisoned`
  tracking for coroutines that panicked mid-transition.
- `Iter` adapter implementing `Iterator` (and `FusedIterator`) for
  `resume = ()` coroutines, and a generated `IntoIterator` impl so such
  coroutines can be passed directly to `for` loops.
- `#[derive(...)]` forwarding onto the generated state enum, enabling
  `Clone` snapshots and serde persistence of suspended coroutines.
- Source fingerprinting: `State::FINGERPRINT`, the `fingerprint`
  attribute flag, the `Fingerprinted` trait, and `FingerprintMismatch`
  for validating persisted states against the coroutine source.
- `no_std` support (no `alloc` required).

[Unreleased]: https://github.com/todesking/diapause/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/todesking/diapause/releases/tag/v0.1.0
