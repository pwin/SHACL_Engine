#!/usr/bin/env python3
"""Times this engine against pySHACL on the generated datasets.

Both engines are measured as whole processes, because that is what a user
actually waits for: parsing the RDF is a real part of the work and neither
engine can skip it. The Rust engine additionally reports its internal
load/compile/validate split on stderr, which is recorded separately.

Correctness is checked before timing. A benchmark of two engines that disagree
measures nothing, so a mismatch in the result count fails the run.
"""

import argparse
import pathlib
import re
import subprocess
import sys
import time

RESULTS_RE = re.compile(r"Results \((\d+)\)")
TIMING_RE = re.compile(
    r"load ([\d.]+)s\s+compile ([\d.]+)s\s+validate ([\d.]+)s.*?(\d+) triples"
)


def best_of(cmd: list[str], runs: int, timeout: float) -> tuple[float, str]:
    """Runs `cmd` `runs` times, returning the best wall time and last stderr."""
    best = float("inf")
    err = ""
    for _ in range(runs):
        start = time.perf_counter()
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        best = min(best, time.perf_counter() - start)
        err = proc.stderr
        # Exit 1 means "does not conform", which is expected here. Anything
        # else is a genuine failure and must not be reported as a time.
        if proc.returncode not in (0, 1):
            raise RuntimeError(f"{cmd[0]} failed: {proc.stderr[:400]}")
        globals()["_last_stdout"] = proc.stdout
    return best, err


def count_ours(stderr: str) -> int | None:
    m = re.search(r"(\d+) results", stderr)
    return int(m.group(1)) if m else None


def count_pyshacl(stdout: str) -> int | None:
    m = RESULTS_RE.search(stdout)
    return int(m.group(1)) if m else None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bench-dir", type=pathlib.Path, required=True)
    ap.add_argument("--shacl", type=pathlib.Path, required=True)
    ap.add_argument("--pyshacl", type=pathlib.Path, required=True)
    ap.add_argument("--sizes", type=int, nargs="+", default=[1000, 10000, 100000])
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--timeout", type=float, default=1800)
    args = ap.parse_args()

    shapes = args.bench_dir / "shapes.ttl"
    rows = []

    for n in args.sizes:
        data = args.bench_dir / f"data-{n}.ttl"
        if not data.exists():
            print(f"skipping {n}: {data} missing", file=sys.stderr)
            continue

        ours_cmd = [
            str(args.shacl), "-d", str(data), "-s", str(shapes), "--quiet", "--timing"
        ]
        py_cmd = [
            str(args.pyshacl), "-s", str(shapes), "-f", "human", str(data)
        ]

        print(f"[{n}] ours ...", file=sys.stderr, flush=True)
        ours_t, ours_err = best_of(ours_cmd, args.runs, args.timeout)
        ours_n = count_ours(ours_err)
        split = TIMING_RE.search(ours_err)

        print(f"[{n}] pyshacl ...", file=sys.stderr, flush=True)
        try:
            py_t, _ = best_of(py_cmd, args.runs, args.timeout)
            py_n = count_pyshacl(globals().get("_last_stdout", ""))
        except subprocess.TimeoutExpired:
            py_t, py_n = float("inf"), None

        if ours_n is not None and py_n is not None and ours_n != py_n:
            print(
                f"MISMATCH at n={n}: ours {ours_n} results, pyshacl {py_n}",
                file=sys.stderr,
            )
            return 1

        rows.append(
            {
                "n": n,
                "triples": int(split.group(4)) if split else 0,
                "ours": ours_t,
                "pyshacl": py_t,
                "ours_validate": float(split.group(3)) if split else 0.0,
                "ours_load": float(split.group(1)) if split else 0.0,
                "results": ours_n,
            }
        )

    print()
    print(f"{'instances':>10} {'triples':>9} {'results':>8} "
          f"{'ours':>9} {'pyshacl':>10} {'speedup':>8} {'ours:validate':>14}")
    print("-" * 74)
    for r in rows:
        speed = "timeout" if r["pyshacl"] == float("inf") else f"{r['pyshacl'] / r['ours']:.1f}x"
        py = "timeout" if r["pyshacl"] == float("inf") else f"{r['pyshacl']:.3f}s"
        print(
            f"{r['n']:>10} {r['triples']:>9} {r['results']:>8} "
            f"{r['ours']:>8.3f}s {py:>10} {speed:>8} {r['ours_validate']:>13.4f}s"
        )
    print()
    print("ours / pyshacl are whole-process wall times, best of "
          f"{args.runs}; ours:validate excludes parsing and shape compilation.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
