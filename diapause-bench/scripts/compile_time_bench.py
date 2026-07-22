#!/usr/bin/env python3
"""Compile-time benchmark: build a crate containing many coroutines.

Generates three standalone crates, each defining N structurally
identical resume-value coroutines (only constants differ):

- ``dia``:  ``#[diapause::coroutine]`` functions
- ``ga``:   genawaiter ``rc::Gen`` generator functions
- ``hand``: handwritten state machines (no dependencies)

For each crate the dependencies are built once to warm the target
directory, then the leaf crate alone is rebuilt (``touch src/lib.rs``)
``--runs`` times in dev and release mode, and the wall-clock time of
each rebuild is reported. This isolates the cost of macro expansion +
codegen of the coroutine-heavy crate itself.

Usage::

    python3 compile_time_bench.py --out /tmp/compile-bench [--n 100] [--runs 3]
"""

import argparse
import pathlib
import shutil
import statistics
import subprocess
import time

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]

DIA_FN = """
#[diapause::coroutine(yield = u64, resume = u64)]
pub fn co_{i}(n: u64) -> u64 {{
    let mut sum: u64 = {i};
    for k in 0..n {{
        let bonus = yield_!(sum);
        if k % 3 == 0 {{
            sum = sum.wrapping_add(k).wrapping_add(bonus);
        }} else {{
            sum = sum.wrapping_mul({i}u64 | 1).wrapping_add(bonus);
        }}
    }}
    sum
}}
"""

GA_FN = """
pub fn co_{i}(n: u64) -> Gen<u64, u64, impl Future<Output = u64>> {{
    Gen::new(move |co| async move {{
        let mut sum: u64 = {i};
        for k in 0..n {{
            let bonus = co.yield_(sum).await;
            if k % 3 == 0 {{
                sum = sum.wrapping_add(k).wrapping_add(bonus);
            }} else {{
                sum = sum.wrapping_mul({i}u64 | 1).wrapping_add(bonus);
            }}
        }}
        sum
    }})
}}
"""

HAND_FN = """
pub struct Co{i} {{
    n: u64,
    k: u64,
    sum: u64,
    state: u8,
}}

pub fn co_{i}(n: u64) -> Co{i} {{
    Co{i} {{ n, k: 0, sum: {i}, state: 0 }}
}}

impl Co{i} {{
    pub fn start(&mut self) -> Option<u64> {{
        assert_eq!(self.state, 0);
        if self.k < self.n {{
            self.state = 1;
            Some(self.sum)
        }} else {{
            self.state = 2;
            None
        }}
    }}

    pub fn resume(&mut self, bonus: u64) -> Option<u64> {{
        assert_eq!(self.state, 1);
        if self.k % 3 == 0 {{
            self.sum = self.sum.wrapping_add(self.k).wrapping_add(bonus);
        }} else {{
            self.sum = self.sum.wrapping_mul({i}u64 | 1).wrapping_add(bonus);
        }}
        self.k += 1;
        if self.k < self.n {{
            Some(self.sum)
        }} else {{
            self.state = 2;
            None
        }}
    }}
}}
"""

STYLES = {
    "dia": {
        "deps": f'diapause = {{ path = "{REPO_ROOT}/diapause" }}',
        "header": "",
        "fn": DIA_FN,
    },
    "ga": {
        "deps": 'genawaiter = "0.99"',
        "header": "use std::future::Future;\nuse genawaiter::rc::Gen;\n",
        "fn": GA_FN,
    },
    "hand": {
        "deps": "",
        "header": "",
        "fn": HAND_FN,
    },
}

CARGO_TOML = """[package]
name = "compile-bench-{style}"
version = "0.0.0"
edition = "2024"

[dependencies]
{deps}

[workspace]
"""


def generate(out: pathlib.Path, style: str, n: int) -> pathlib.Path:
    crate = out / style
    if crate.exists():
        shutil.rmtree(crate)
    (crate / "src").mkdir(parents=True)
    cfg = STYLES[style]
    (crate / "Cargo.toml").write_text(
        CARGO_TOML.format(style=style, deps=cfg["deps"])
    )
    body = cfg["header"] + "".join(cfg["fn"].format(i=i) for i in range(n))
    (crate / "src" / "lib.rs").write_text(body)
    return crate


def cargo_build(crate: pathlib.Path, release: bool) -> None:
    cmd = ["cargo", "build", "--quiet"]
    if release:
        cmd.append("--release")
    subprocess.run(cmd, cwd=crate, check=True)


def bench(crate: pathlib.Path, release: bool, runs: int) -> list[float]:
    cargo_build(crate, release)  # warm dependencies
    times = []
    lib = crate / "src" / "lib.rs"
    for _ in range(runs):
        lib.touch()
        t0 = time.perf_counter()
        cargo_build(crate, release)
        times.append(time.perf_counter() - t0)
    return times


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True, type=pathlib.Path)
    ap.add_argument("--n", type=int, default=100)
    ap.add_argument("--runs", type=int, default=3)
    args = ap.parse_args()

    rustc = subprocess.run(
        ["rustc", "--version"], capture_output=True, text=True, check=True
    ).stdout.strip()
    print(f"# {rustc}, n = {args.n} coroutines, {args.runs} rebuilds each")
    print(f"# leaf-crate rebuild wall time (deps warm), seconds")
    for style in STYLES:
        crate = generate(args.out, style, args.n)
        for release in (False, True):
            times = bench(crate, release, args.runs)
            mode = "release" if release else "dev"
            joined = ", ".join(f"{t:.2f}" for t in times)
            print(
                f"{style:5} {mode:8} median {statistics.median(times):6.2f}  "
                f"runs [{joined}]"
            )


if __name__ == "__main__":
    main()
