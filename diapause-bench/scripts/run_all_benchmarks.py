#!/usr/bin/env python3
"""Run every measurement behind docs/benchmarks.md in one shot.

Stages (each can be skipped with ``--skip``):

- ``runtime``: ``cargo bench -p diapause-bench``, then a summary table
  from criterion's ``target/criterion/*/*/new/estimates.json`` (slope
  point estimate, falling back to mean), fastest per row in bold.
- ``expand``:  source vs. ``cargo expand`` size of the ``dia`` /
  ``hand`` / ``ga`` workload modules (requires cargo-expand; skipped
  with a note if missing).
- ``size``:    release ``__text`` section and stripped-file sizes of
  the ``size_*`` example binaries, as deltas over ``size_baseline``.
- ``compile``: ``compile_time_bench.py`` for each ``--compile-n``,
  medians parsed into one table.

Raw logs land in the output directory next to ``summary.md``, whose
tables mirror the layout of docs/benchmarks.md so results can be
copied over directly.

Usage::

    python3 diapause-bench/scripts/run_all_benchmarks.py
    python3 ... --skip compile,expand
    python3 ... --runtime-from-existing   # summarize a previous bench run
"""

import argparse
import datetime
import json
import pathlib
import re
import shutil
import subprocess
import sys
import time

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
BENCH_CRATE = REPO_ROOT / "diapause-bench"

STAGES = ("runtime", "expand", "size", "compile")

# Row/column order of the runtime table in docs/benchmarks.md.
WORKLOADS = ("counter", "nested", "running_total", "large_state")
IMPLS = (
    "diapause",
    "handwritten",
    "genawaiter_rc",
    "genawaiter_stack",
    "corosensei",
    "generator",
    "next_gen",
)

EXPAND_MODULES = ("dia", "hand", "ga")

SIZE_EXAMPLES = ("size_baseline", "size_diapause", "size_handwritten", "size_genawaiter")

COMPILE_STYLES = {"dia": "diapause", "ga": "genawaiter", "hand": "handwritten"}


def run(cmd, log: pathlib.Path | None = None, cwd=REPO_ROOT, check=True) -> str:
    """Run a command, mirror its output to ``log``, return stdout."""
    print(f"$ {' '.join(map(str, cmd))}", flush=True)
    proc = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    if log:
        log.write_text(proc.stdout + proc.stderr)
    if check and proc.returncode != 0:
        sys.stderr.write(proc.stdout + proc.stderr)
        raise SystemExit(f"command failed ({proc.returncode}): {' '.join(map(str, cmd))}")
    return proc.stdout


def try_output(cmd) -> str | None:
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True)
    except OSError:
        return None
    return proc.stdout.strip() if proc.returncode == 0 else None


def fmt_us(ns: float) -> str:
    """Nanoseconds to a 3-significant-digit µs string (doc style)."""
    us = ns / 1000.0
    if us >= 100:
        return f"{us:.0f} µs"
    if us >= 10:
        return f"{us:.1f} µs"
    return f"{us:.2f} µs"


def environment() -> list[str]:
    git = try_output(["git", "-C", str(REPO_ROOT), "rev-parse", "--short", "HEAD"]) or "?"
    dirty = try_output(["git", "-C", str(REPO_ROOT), "status", "--porcelain"])
    if dirty:
        git += " (dirty)"
    lines = [
        f"- Date: {datetime.date.today().isoformat()}",
        f"- Code: diapause commit `{git}`",
        f"- Toolchain: {try_output(['rustc', '--version']) or '?'}",
    ]
    cpu = try_output(["sysctl", "-n", "machdep.cpu.brand_string"])
    mem = try_output(["sysctl", "-n", "hw.memsize"])
    os_ver = try_output(["sw_vers", "-productVersion"])
    host = try_output(["rustc", "-vV"])
    target = ""
    if host:
        m = re.search(r"^host: (.+)$", host, re.M)
        target = f" (`{m.group(1)}`)" if m else ""
    # sysctl may be unavailable (e.g. sandboxed runs); fall back to uname.
    hw = cpu or try_output(["uname", "-m"]) or "?"
    mem_gb = f", {int(mem) // 2**30} GB RAM" if mem else ""
    os_part = f", macOS {os_ver}" if os_ver else ""
    lines.append(f"- Machine: {hw}{mem_gb}{os_part}{target}")
    crit = re.search(
        r'name = "criterion"\nversion = "([^"]+)"',
        (REPO_ROOT / "Cargo.lock").read_text(),
    )
    if crit:
        lines.append(f"- Harness: criterion {crit.group(1)}, default settings")
    return lines


# === runtime ===


def stage_runtime(out: pathlib.Path, from_existing: bool) -> list[str]:
    if not from_existing:
        run(["cargo", "bench", "-p", "diapause-bench"], log=out / "runtime.log")
    rows = []
    for wl in WORKLOADS:
        cells = []
        for impl in IMPLS:
            est = REPO_ROOT / "target" / "criterion" / wl / impl / "new" / "estimates.json"
            if not est.exists():
                cells.append(None)
                continue
            d = json.loads(est.read_text())
            e = d.get("slope") or d["mean"]
            cells.append(e["point_estimate"])
        fastest = min((c for c in cells if c is not None), default=None)
        rendered = [
            "—" if c is None else (f"**{fmt_us(c)}**" if c == fastest else fmt_us(c))
            for c in cells
        ]
        rows.append(f"| `{wl}` | " + " | ".join(rendered) + " |")
    header = "| workload | " + " | ".join(IMPLS) + " |"
    sep = "|---" * (len(IMPLS) + 1) + "|"
    return [header, sep, *rows]


# === expand ===


def module_source_span(src: str, name: str) -> str | None:
    """The text of ``pub mod <name> { ... }`` in ``src``, by brace
    counting (does not account for braces in strings/comments, which
    the workload modules do not contain in a way that unbalances)."""
    m = re.search(rf"^pub mod {name} \{{", src, re.M)
    if not m:
        return None
    depth = 0
    for i in range(m.end() - 1, len(src)):
        if src[i] == "{":
            depth += 1
        elif src[i] == "}":
            depth -= 1
            if depth == 0:
                return src[m.start() : i + 1]
    return None


def stage_expand(out: pathlib.Path) -> list[str]:
    if try_output(["cargo", "expand", "--version"]) is None:
        return ["cargo-expand not installed — stage skipped (`cargo install cargo-expand`)."]
    src = (BENCH_CRATE / "src" / "lib.rs").read_text()
    rows = []
    for mod in EXPAND_MODULES:
        expanded = run(
            ["cargo", "expand", "-p", "diapause-bench", mod],
            log=out / f"expand-{mod}.rs",
        )
        span = module_source_span(src, mod)
        src_cell = (
            f"{span.count(chr(10)) + 1} lines / {len(span.encode()) / 1000:.1f} kB"
            if span
            else "?"
        )
        exp_cell = (
            f"{expanded.count(chr(10))} lines / {len(expanded.encode()) / 1000:.1f} kB"
        )
        rows.append(f"| `{mod}` | {src_cell} | {exp_cell} |")
    return ["| module | source | expanded |", "|---|---|---|", *rows]


# === size ===


def text_section(binary: pathlib.Path) -> int:
    out = run(["size", "-m", str(binary)])
    m = re.search(r"Section __text: (\d+)", out)
    if not m:
        raise SystemExit(f"no __text section reported for {binary}")
    return int(m.group(1))


def stripped_size(binary: pathlib.Path, scratch: pathlib.Path) -> int:
    copy = scratch / binary.name
    shutil.copy2(binary, copy)
    run(["strip", str(copy)], check=False)  # strip warns on signed binaries; sizes still apply
    return copy.stat().st_size


def stage_size(out: pathlib.Path) -> list[str]:
    run(
        ["cargo", "build", "--release", "--examples", "-p", "diapause-bench"],
        log=out / "size-build.log",
    )
    scratch = out / "stripped"
    scratch.mkdir(exist_ok=True)
    bindir = REPO_ROOT / "target" / "release" / "examples"
    text = {ex: text_section(bindir / ex) for ex in SIZE_EXAMPLES}
    stripped = {ex: stripped_size(bindir / ex, scratch) for ex in SIZE_EXAMPLES}
    def grp(n: int, sign: bool = False) -> str:
        return format(n, "+_" if sign else "_").replace("_", " ")

    base = SIZE_EXAMPLES[0]
    rows = [f"| `{base}` | {grp(text[base])} | — | — |"]
    for ex in SIZE_EXAMPLES[1:]:
        rows.append(
            f"| `{ex}` | {grp(text[ex])} | {grp(text[ex] - text[base], sign=True)} | "
            f"{grp(stripped[ex] - stripped[base], sign=True)} |"
        )
    return [
        "| binary | `__text` bytes | delta over baseline | stripped file size delta |",
        "|---|---|---|---|",
        *rows,
    ]


# === compile ===


def stage_compile(out: pathlib.Path, ns: list[int], runs: int) -> list[str]:
    rows = []
    for n in ns:
        log = out / f"compile-n{n}.log"
        stdout = run(
            [
                sys.executable,
                str(BENCH_CRATE / "scripts" / "compile_time_bench.py"),
                "--out",
                str(out / f"compile-crates-n{n}"),
                "--n",
                str(n),
                "--runs",
                str(runs),
            ],
            log=log,
        )
        # Lines look like: ``dia   dev      median   0.20  runs [...]``.
        medians: dict[tuple[str, str], str] = {}
        for style, mode, med in re.findall(
            r"^(\w+)\s+(dev|release)\s+median\s+([\d.]+)", stdout, re.M
        ):
            medians[(COMPILE_STYLES.get(style, style), mode)] = med
        for mode in ("dev", "release"):
            cells = [
                medians.get((impl, mode), "?")
                for impl in ("diapause", "genawaiter", "handwritten")
            ]
            rows.append(f"| {n} | {mode} | " + " | ".join(cells) + " |")
    return [
        "| N | mode | diapause | genawaiter | handwritten |",
        "|---|---|---|---|---|",
        *rows,
    ]


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--out", type=pathlib.Path, help="output directory (default: target/bench-results/<date>-<commit>)")
    ap.add_argument("--skip", default="", help=f"comma-separated stages to skip ({', '.join(STAGES)})")
    ap.add_argument("--runtime-from-existing", action="store_true",
                    help="summarize target/criterion from a previous run instead of re-running cargo bench")
    ap.add_argument("--compile-n", default="100,500", help="coroutine counts for the compile stage (default: 100,500)")
    ap.add_argument("--compile-runs", type=int, default=3, help="rebuilds per compile measurement (default: 3)")
    args = ap.parse_args()

    skip = {s.strip() for s in args.skip.split(",") if s.strip()}
    if unknown := skip - set(STAGES):
        raise SystemExit(f"unknown stage(s) in --skip: {', '.join(sorted(unknown))}")

    git = try_output(["git", "-C", str(REPO_ROOT), "rev-parse", "--short", "HEAD"]) or "nogit"
    out = args.out or REPO_ROOT / "target" / "bench-results" / f"{datetime.date.today().isoformat()}-{git}"
    out.mkdir(parents=True, exist_ok=True)

    sections = [("Measurement conditions", environment())]
    stage_fns = {
        "runtime": ("1. Runtime throughput", lambda: stage_runtime(out, args.runtime_from_existing)),
        "expand": ("2a. Macro expansion size", lambda: stage_expand(out)),
        "size": ("2b. Machine code size (release `__text`)", lambda: stage_size(out)),
        "compile": (
            "3. Compile time (median rebuild seconds)",
            lambda: stage_compile(out, [int(n) for n in args.compile_n.split(",")], args.compile_runs),
        ),
    }
    for stage in STAGES:
        title, fn = stage_fns[stage]
        if stage in skip:
            print(f"== {stage}: skipped")
            continue
        print(f"== {stage}")
        t0 = time.perf_counter()
        body = fn()
        body.append(f"\n_({time.perf_counter() - t0:.0f} s)_")
        sections.append((title, body))

    summary = "# Benchmark results\n\n" + "\n\n".join(
        f"## {title}\n\n" + "\n".join(body) for title, body in sections
    ) + "\n"
    (out / "summary.md").write_text(summary)
    print(f"\n{summary}")
    print(f"summary: {out / 'summary.md'}")


if __name__ == "__main__":
    main()
