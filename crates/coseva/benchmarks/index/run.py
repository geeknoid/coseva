#!/usr/bin/env python3
"""Measure and gate wall-clock index construction on one pinned environment."""

from __future__ import annotations

import argparse
import json
import os
import platform
import subprocess
import sys
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from harness import (  # noqa: E402
    benchmark_fingerprint,
    collect,
    dirty_paths,
    emit,
    environment_key,
    git,
    host_metadata,
    measure,
    parse_rustc,
)

HARNESS_DIR = Path(__file__).resolve().parent
REPOSITORY = HARNESS_DIR.parents[3]
BENCHMARK_FILE = Path("crates/coseva/benches/index_build_wallclock.rs")
DEFAULT_BASELINE = HARNESS_DIR / "baseline.json"
GROUP = "index_build_wallclock"
FEATURES = "std,index,parallel,benchmarking"
SIZES_MIB = (2, 4, 8, 32, 64)
# `PARALLEL_INDEX_THRESHOLD_BYTES` in `src/index/csv_index.rs`, in mebibytes.
# `CsvIndex::build` takes the parallel builder from this size upwards, and this
# harness exists to check that the constant still names the right size.
THRESHOLD_MIB = 8
GATE_CASE = f"{GROUP}/parallel/threads-auto/64MiB"
# Recorded and reported, but not blocking, for the reason `benchmarks/parallel/
# run.py` documents at length: an absolute wall-clock number is pinned to a host
# by the environment key, and the environment key cannot see what else that host
# is running. Measured drift there was 12-18.5% across every case in a pair of
# runs on unchanged code. The speedup floors and the routing check below are
# ratios taken within a single run, so they are immune to it, and they are what
# fails this harness.
GATE_BLOCKING = False
# A speedup is a ratio between two cases measured in the same run on the same
# document, so unlike the absolute throughput above it survives the move to any
# machine. It is also the entire justification for the parallel builder, and
# for the threshold constant that decides when to use it.
#
# The reference host measured 1.24x at 2 MiB, 1.79x at 4 MiB, then 2.26x, 1.99x
# and 2.26x at 8, 32 and 64 MiB. Only the sizes at or above the threshold are
# gated: below it the crate deliberately does not use the parallel builder, so
# a ratio there describes a path production never takes and is recorded for
# whoever next revisits the constant rather than blocking a merge.
#
# The floors are set 20% under the recorded ratios, which is far outside their
# observed spread and still far above 1.0 -- losing the builder's parallelism
# would take these towards 1.0, a fall of more than half.
SPEEDUP_MARGIN_PERCENT = 20.0
# How far `dispatched` may sit from the builder the threshold says it picks.
# The two builders differ by roughly a factor of two at every size, and the
# reference host put `dispatched` within 5.1% of its expected builder, so this
# band cannot be crossed by noise -- only by the threshold routing to the other
# side.
ROUTING_TOLERANCE_PERCENT = 25.0
METRIC = "criterion_median_throughput_mib_per_second"
SCHEMA = 1


def serial_id(size: int) -> str:
    return f"{GROUP}/serial/{size}MiB"


def parallel_id(size: int) -> str:
    return f"{GROUP}/parallel/threads-auto/{size}MiB"


def dispatched_id(size: int) -> str:
    return f"{GROUP}/dispatched/{size}MiB"


def required_ids() -> list[str]:
    return [
        point_id(size)
        for size in SIZES_MIB
        for point_id in (serial_id, parallel_id, dispatched_id)
    ]


def speedups(points: dict[str, Any]) -> dict[str, float]:
    """Serial-over-parallel elapsed time, per size, from one run."""
    return {
        f"{size}MiB": float(points[serial_id(size)]["median_ns"])
        / float(points[parallel_id(size)]["median_ns"])
        for size in SIZES_MIB
    }


def blocking_sizes() -> dict[str, bool]:
    return {f"{size}MiB": size >= THRESHOLD_MIB for size in SIZES_MIB}


def routing(points: dict[str, Any]) -> dict[str, dict[str, Any]]:
    """Check `CsvIndex::build` reaches the builder the threshold promises.

    Each size compares the dispatched case against both forced builders. The
    expected one is decided by the threshold constant alone, so if the constant
    and the code that reads it ever disagree, this is what notices.
    """
    report: dict[str, dict[str, Any]] = {}
    for size in SIZES_MIB:
        parallel_expected = size >= THRESHOLD_MIB
        dispatched = float(points[dispatched_id(size)]["median_ns"])
        expected = float(
            points[(parallel_id if parallel_expected else serial_id)(size)]["median_ns"]
        )
        other = float(
            points[(serial_id if parallel_expected else parallel_id)(size)]["median_ns"]
        )
        deviation = abs(dispatched / expected - 1.0) * 100.0
        report[f"{size}MiB"] = {
            "expected_builder": "parallel" if parallel_expected else "serial",
            "deviation_percent": round(deviation, 3),
            "tolerance_percent": ROUTING_TOLERANCE_PERCENT,
            "passed": deviation <= ROUTING_TOLERANCE_PERCENT
            and abs(dispatched - expected) < abs(dispatched - other),
        }
    return report


def crossover_mib(observed: dict[str, float]) -> int | None:
    """The smallest size from which the parallel builder wins at every size."""
    return next(
        (
            size
            for index, size in enumerate(SIZES_MIB)
            if all(observed[f"{later}MiB"] > 1.0 for later in SIZES_MIB[index:])
        ),
        None,
    )


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(
        description=__doc__,
        epilog=(
            "Use an otherwise-idle named host with a pinned toolchain. A "
            "different environment is reported as non-comparable and cannot "
            "fail the absolute gate; the speedup floors and the routing check "
            "are ratios and hold everywhere."
        ),
    )
    result.add_argument("--baseline", type=Path, default=DEFAULT_BASELINE)
    result.add_argument("--output", type=Path)
    result.add_argument("--samples", type=int, default=20)
    result.add_argument("--warm-up-time", type=float, default=1.0)
    result.add_argument("--measurement-time", type=float, default=5.0)
    result.add_argument(
        "--host-id",
        default=os.environ.get("COSEVA_PARALLEL_BENCH_HOST", platform.node()),
    )
    result.add_argument("--source-root", type=Path, default=REPOSITORY)
    result.add_argument(
        "--target-dir", type=Path, default=REPOSITORY / "target/index-bench"
    )
    result.add_argument("--allow-dirty", action="store_true")
    result.add_argument(
        "--ignore-environment",
        action="store_true",
        help="run diagnostics elsewhere, but keep the result non-comparable",
    )
    result.add_argument("--skip-run", action="store_true")
    result.add_argument("--write-baseline", action="store_true")
    return result


def main() -> int:
    args = parser().parse_args()
    if args.samples < 10:
        raise SystemExit("--samples must be at least 10 (Criterion requirement)")
    source_root = args.source_root.resolve()
    target_dir = args.target_dir.resolve()
    revision = git(source_root, "rev-parse", "HEAD")
    dirty = dirty_paths(source_root)
    runtime_dirty = [path for path in dirty if path != str(BENCHMARK_FILE)]
    if runtime_dirty and not args.allow_dirty:
        raise SystemExit(
            "runtime source tree is dirty; use a clean checkout or --allow-dirty "
            "for a non-canonical diagnostic run"
        )

    host = host_metadata(args.host_id)
    toolchain = parse_rustc(source_root)
    env_key = environment_key(host, toolchain)
    fingerprint = benchmark_fingerprint(source_root, BENCHMARK_FILE)
    baseline: dict[str, Any] | None = None
    comparable = True
    if not args.write_baseline:
        baseline = json.loads(args.baseline.read_text(encoding="utf-8"))
        if baseline["schema"] != SCHEMA or baseline["metric"] != METRIC:
            raise SystemExit(f"unsupported baseline schema or metric in {args.baseline}")
        if baseline["benchmark"]["fingerprint"] != fingerprint:
            raise SystemExit(
                "index benchmark differs from the explicit baseline; "
                "refresh it on the pinned environment"
            )
        comparable = baseline["environment_key"] == env_key
        if not comparable and not args.ignore_environment:
            emit(
                {
                    "schema": SCHEMA,
                    "metric": METRIC,
                    "status": "non_comparable",
                    "comparable": False,
                    "reason": "environment_key_mismatch",
                    "environment_key": env_key,
                    "baseline_environment_key": baseline["environment_key"],
                    "host": host,
                    "toolchain": toolchain,
                },
                args.output,
            )
            return 0

    if not args.skip_run:
        measure(
            source_root,
            target_dir,
            bench="index_build_wallclock",
            groups=(GROUP,),
            features=FEATURES,
            warm_up=args.warm_up_time,
            measurement=args.measurement_time,
            samples=args.samples,
        )
    points = collect(target_dir, GROUP, required_ids())
    observed = speedups(points)
    blocking = blocking_sizes()

    if args.write_baseline:
        result = {
            "schema": SCHEMA,
            "metric": METRIC,
            "threshold_percent": 10.0,
            "samples": args.samples,
            "warm_up_seconds": args.warm_up_time,
            "measurement_seconds": args.measurement_time,
            "environment_key": env_key,
            "host": host,
            "toolchain": toolchain,
            "source": {
                "revision": revision,
                "runtime_tree_clean": not runtime_dirty,
                "benchmark_overlay": str(BENCHMARK_FILE) in dirty,
            },
            "benchmark": {
                "fingerprint": fingerprint,
                "sizes_mib": list(SIZES_MIB),
                "threshold_mib": THRESHOLD_MIB,
            },
            "gate": {
                "case": GATE_CASE,
                "baseline_mib_per_second": points[GATE_CASE][
                    "median_mib_per_second"
                ],
            },
            "speedup_floors": {
                "margin_percent": SPEEDUP_MARGIN_PERCENT,
                "cases": {
                    size: {
                        "measured": round(value, 3),
                        "floor": round(
                            value * (1.0 - SPEEDUP_MARGIN_PERCENT / 100.0), 3
                        ),
                        "blocking": blocking[size],
                    }
                    for size, value in observed.items()
                },
            },
            "routing": routing(points),
            "crossover_mib": crossover_mib(observed),
            "points": points,
        }
        args.baseline.parent.mkdir(parents=True, exist_ok=True)
        args.baseline.write_text(
            json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        print(f"wrote {args.baseline}", file=sys.stderr)
        return 0

    assert baseline is not None
    threshold = float(baseline["threshold_percent"])
    baseline_throughput = float(baseline["gate"]["baseline_mib_per_second"])
    current_throughput = float(points[GATE_CASE]["median_mib_per_second"])
    minimum_throughput = baseline_throughput * (1.0 - threshold / 100.0)
    passed = current_throughput >= minimum_throughput

    floors = baseline.get("speedup_floors", {}).get("cases", {})
    speedup_report = {}
    speedups_passed = True
    for size, value in observed.items():
        floor = floors.get(size, {}).get("floor")
        held = floor is None or value >= float(floor)
        if blocking[size]:
            speedups_passed &= held
        speedup_report[size] = {
            "measured": round(value, 3),
            "floor": floor,
            "blocking": blocking[size],
            "passed": held,
        }

    routing_report = routing(points)
    routing_passed = all(case["passed"] for case in routing_report.values())

    status = "pass" if (passed or not GATE_BLOCKING) else "fail"
    if not comparable:
        status = "non_comparable"
    if not (speedups_passed and routing_passed):
        status = "fail"
    result = {
        "schema": SCHEMA,
        "metric": METRIC,
        "threshold_percent": threshold,
        "status": status,
        "comparable": comparable,
        "canonical": comparable and not runtime_dirty,
        "generated_at": datetime.now(UTC).isoformat(),
        "environment_key": env_key,
        "host": host,
        "toolchain": toolchain,
        "source": {
            "revision": revision,
            "runtime_tree_clean": not runtime_dirty,
            "dirty_entry_count": len(dirty),
        },
        "baseline": {
            "path": str(args.baseline),
            "revision": baseline["source"]["revision"],
            "environment_key": baseline["environment_key"],
        },
        "gate": {
            "case": GATE_CASE,
            "baseline_mib_per_second": baseline_throughput,
            "current_mib_per_second": current_throughput,
            "minimum_mib_per_second": round(minimum_throughput, 3),
            "throughput_regression_percent": round(
                (1.0 - current_throughput / baseline_throughput) * 100.0, 3
            ),
            "passed": passed if comparable else None,
            "blocking": GATE_BLOCKING,
        },
        "speedup_floors": {
            "margin_percent": baseline.get("speedup_floors", {}).get(
                "margin_percent", SPEEDUP_MARGIN_PERCENT
            ),
            "passed": speedups_passed,
            "cases": speedup_report,
        },
        "routing": {"passed": routing_passed, "cases": routing_report},
        "crossover_mib": crossover_mib(observed),
        "points": points,
    }
    emit(result, args.output)
    return 0 if status != "fail" else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except subprocess.CalledProcessError as error:
        sys.stderr.write(error.stderr or "")
        raise SystemExit(error.returncode) from error
