#!/usr/bin/env python3
"""Measure and gate ordered parallel parsing on one pinned environment."""

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
    collect as collect_points,
    dirty_paths,
    emit,
    environment_key,
    git,
    host_metadata,
    measure as measure_bench,
    parse_rustc,
)

HARNESS_DIR = Path(__file__).resolve().parent
REPOSITORY = HARNESS_DIR.parents[3]
BENCHMARK_FILE = Path("crates/coseva/benches/parallel.rs")
DEFAULT_BASELINE = HARNESS_DIR / "baseline.json"
SIZES_MIB = (8, 16, 32, 64)
THREADS = ("2", "4", "8", "auto")
BATCH_RECORDS = (32, 256, 4096, 16384)
DEFAULT_BATCH_RECORDS = 4096
# `benches/parallel.rs` defines a second Criterion group, `skew`, over one
# document whose parse cost is deliberately uneven. It exists to price the owned
# path's static chunk-to-worker rotation against the borrowed path's shared
# cursor on an input where the two rules differ, which is the measurement P2
# needs. Both groups are cleared before a run and collected after it; a group
# missing from here would silently keep reporting its last measurement.
GROUPS = ("parallel", "skew")
SKEW_SIZE_MIB = 32
SKEW_THREADS = ("2", "4", "8")
GATE_CASE = "parallel/owned/threads-auto/64MiB"
# The absolute throughput of `GATE_CASE` is recorded and reported, but it does
# not block.
#
# It used to. It was pinned to the recorded reference host on the theory that
# the environment key made it reproducible -- but the key describes the machine,
# not what else the machine is doing. Two runs of identical code on that host,
# twenty minutes apart, moved every 64 MiB case together by 12-18.5% against
# this gate's 10% threshold: the owned case by 18.5%, the borrowed fold by
# 16.9%, and both serial baselines by 12.0% and 16.6%. That is the box being
# busy, and no absolute number survives it.
#
# The derived speedups over those same two runs held to a few percent, because
# both sides of a ratio move together. So the ratios below are what block, and
# this is a diagnostic: read it only from a run that reports `canonical` and was
# taken on an idle host.
GATE_BLOCKING = False
# A speedup is a ratio between two cases measured in the same run, so it does
# not depend on the host, and it is the property the parallel path exists to
# provide.
#
# The borrowed floor is derived from the recorded measurement less
# `SPEEDUP_MARGIN_PERCENT`: four full runs on the reference host put it at
# 2.30-2.56 (median 2.43, +/-5.5%), so a 20% margin is four times its observed
# spread and still far inside the regression it exists to catch -- losing the
# borrowed path's parallelism would take the ratio towards 1.0, a 59% fall.
#
# The owned floor cannot be derived that way. Its measured value swings much
# more than the borrowed one even after normalization, because the owned path is
# a race between the workers and one consumer over a recycling pool. A margin
# below its median would sit under 1.0 and assert nothing. So it is pinned
# instead, by contract rather than by measurement: the ordered owned path exists
# to beat a serial parse, and `FIXED_FLOORS` states the least that can mean.
# That is a weaker claim than the borrowed floor, and it is the strongest one
# that does not fail on noise.
FIXED_FLOORS = {"owned_batch_64MiB": 1.15}
# name, the parallel case, the serial case it is scaled against, and whether it
# blocks. Both block now: the absolute gate above no longer does, so these are
# the harness's only protection.
SPEEDUP_FLOORS = (
    (
        "borrowed_fold_64MiB",
        "parallel/fold/threads-auto/64MiB",
        "parallel/serial/borrowed/64MiB",
        True,
    ),
    (
        "owned_batch_64MiB",
        "parallel/owned/threads-auto/64MiB",
        "parallel/serial/owned/64MiB",
        True,
    ),
)
SPEEDUP_MARGIN_PERCENT = 20.0
METRIC = "criterion_median_throughput_mib_per_second"
SCHEMA = 1


def required_ids() -> list[str]:
    ids = []
    for size in SIZES_MIB:
        ids.extend(
            [
                f"parallel/serial/owned/{size}MiB",
                f"parallel/serial/borrowed/{size}MiB",
            ]
        )
        for threads in THREADS:
            ids.extend(
                [
                    f"parallel/fold/threads-{threads}/{size}MiB",
                    f"parallel/owned/threads-{threads}/{size}MiB",
                ]
            )
        for batch in BATCH_RECORDS:
            if batch != DEFAULT_BATCH_RECORDS:
                ids.append(
                    f"parallel/owned/batch-{batch}/threads-auto/{size}MiB"
                )
    return ids


def skew_required_ids() -> list[str]:
    ids = [
        f"skew/serial/owned/{SKEW_SIZE_MIB}MiB",
        f"skew/serial/borrowed/{SKEW_SIZE_MIB}MiB",
    ]
    for threads in SKEW_THREADS:
        ids.extend(
            [
                f"skew/fold/threads-{threads}/{SKEW_SIZE_MIB}MiB",
                f"skew/owned/threads-{threads}/{SKEW_SIZE_MIB}MiB",
            ]
        )
    return ids


def collect(target_dir: Path) -> dict[str, dict[str, float | int]]:
    return collect_points(target_dir, "parallel", required_ids())


def collect_skew(target_dir: Path) -> dict[str, dict[str, float | int]]:
    return collect_points(target_dir, "skew", skew_required_ids())


def skew_attribution(points: dict[str, Any]) -> dict[str, Any]:
    """Price the static rotation against the shared cursor on uneven chunks.

    Each path is scaled against the serial parse of the same bytes measured in
    the same run, so both figures are ratios and survive a move between hosts --
    which matters more here than usual, since this is the number P2 has to be
    decided on.

    `rotation_cost` is what remains: how far the owned path's scaling falls
    short of the borrowed path's on an input where the only difference between
    them that matters is how chunks reach workers. On the even document the two
    rules are indistinguishable, so a gap here and not there is the rotation's
    price rather than the owned path's general overhead.
    """
    size = f"{SKEW_SIZE_MIB}MiB"
    serial_owned = float(points[f"skew/serial/owned/{size}"]["median_ns"])
    serial_borrowed = float(points[f"skew/serial/borrowed/{size}"]["median_ns"])
    cases: dict[str, dict[str, float]] = {}
    for threads in SKEW_THREADS:
        fold = serial_borrowed / float(points[f"skew/fold/threads-{threads}/{size}"]["median_ns"])
        owned = serial_owned / float(points[f"skew/owned/threads-{threads}/{size}"]["median_ns"])
        cases[f"threads-{threads}"] = {
            "borrowed_speedup": round(fold, 4),
            "owned_speedup": round(owned, 4),
            "rotation_cost": round(1.0 - owned / fold, 4),
        }
    return {"size_mib": SKEW_SIZE_MIB, "cases": cases}


def owned_id(threads: str, size: int, batch: int = DEFAULT_BATCH_RECORDS) -> str:
    if batch == DEFAULT_BATCH_RECORDS:
        return f"parallel/owned/threads-{threads}/{size}MiB"
    return f"parallel/owned/batch-{batch}/threads-{threads}/{size}MiB"


def derived(points: dict[str, dict[str, float | int]]) -> dict[str, Any]:
    ratios: dict[str, dict[str, float]] = {}
    crossovers: dict[str, int | None] = {}

    paths = {
        **{
            f"fold/threads-{threads}": (
                "borrowed",
                lambda size, threads=threads: (
                    f"parallel/fold/threads-{threads}/{size}MiB"
                ),
            )
            for threads in THREADS
        },
        **{
            f"owned/threads-{threads}": (
                "owned",
                lambda size, threads=threads: owned_id(threads, size),
            )
            for threads in THREADS
        },
        **{
            f"owned/batch-{batch}/threads-auto": (
                "owned",
                lambda size, batch=batch: owned_id("auto", size, batch),
            )
            for batch in BATCH_RECORDS
        },
    }
    for label, (serial_kind, point_id) in paths.items():
        path_ratios = {}
        for size in SIZES_MIB:
            serial = points[f"parallel/serial/{serial_kind}/{size}MiB"]["median_ns"]
            parallel = points[point_id(size)]["median_ns"]
            path_ratios[f"{size}MiB"] = round(float(serial) / float(parallel), 4)
        ratios[label] = path_ratios
        crossovers[label] = next(
            (
                size
                for index, size in enumerate(SIZES_MIB)
                if all(
                    path_ratios[f"{later}MiB"] > 1.0
                    for later in SIZES_MIB[index:]
                )
            ),
            None,
        )
    return {"serial_ratios": ratios, "crossover_mib": crossovers}


def speedups(points: dict[str, Any]) -> dict[str, float]:
    return {
        name: points[fast]["median_mib_per_second"]
        / points[slow]["median_mib_per_second"]
        for name, fast, slow, _ in SPEEDUP_FLOORS
    }


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(
        description=__doc__,
        epilog=(
            "Use an otherwise-idle named host with a pinned toolchain. A "
            "different environment is reported as non-comparable and cannot fail the gate."
        ),
    )
    result.add_argument("--baseline", type=Path, default=DEFAULT_BASELINE)
    result.add_argument("--output", type=Path)
    result.add_argument("--samples", type=int, default=20)
    result.add_argument("--warm-up-time", type=float, default=1.0)
    result.add_argument("--measurement-time", type=float, default=3.0)
    result.add_argument(
        "--host-id",
        default=os.environ.get("COSEVA_PARALLEL_BENCH_HOST", platform.node()),
    )
    result.add_argument("--source-root", type=Path, default=REPOSITORY)
    result.add_argument(
        "--target-dir",
        type=Path,
        default=REPOSITORY / "target/parallel-bench",
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
    if args.write_baseline and runtime_dirty:
        print(
            "warning: writing an explicitly allowed baseline from dirty runtime source; "
            "the artifact records this provenance",
            file=sys.stderr,
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
                "parallel benchmark differs from the explicit baseline; "
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
        measure_bench(
            source_root,
            target_dir,
            bench="parallel",
            groups=GROUPS,
            features="std,parallel",
            warm_up=args.warm_up_time,
            measurement=args.measurement_time,
            samples=args.samples,
        )
    points = collect(target_dir)
    attribution = derived(points)
    skew_points = collect_skew(target_dir)
    skew = skew_attribution(skew_points)

    if args.write_baseline:
        gate_throughput = points[GATE_CASE]["median_mib_per_second"]
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
                "threads": list(THREADS),
                "batch_records": list(BATCH_RECORDS),
                "default_batch_records": DEFAULT_BATCH_RECORDS,
            },
            "gate": {
                "case": GATE_CASE,
                "baseline_mib_per_second": gate_throughput,
            },
            "speedup_floors": {
                "margin_percent": SPEEDUP_MARGIN_PERCENT,
                "cases": {
                    name: {
                        "measured": round(value, 3),
                        "floor": FIXED_FLOORS.get(
                            name, round(value * (1.0 - SPEEDUP_MARGIN_PERCENT / 100.0), 3)
                        ),
                        "blocking": gates,
                    }
                    for (name, _, _, gates), value in zip(
                        SPEEDUP_FLOORS, speedups(points).values(), strict=True
                    )
                },
            },
            "points": points,
            "skew_points": skew_points,
            "skew": skew,
            **attribution,
        }
        args.baseline.parent.mkdir(parents=True, exist_ok=True)
        args.baseline.write_text(
            json.dumps(result, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        print(f"wrote {args.baseline}", file=sys.stderr)
        return 0

    assert baseline is not None
    threshold = float(baseline["threshold_percent"])
    baseline_throughput = float(baseline["gate"]["baseline_mib_per_second"])
    current_throughput = float(points[GATE_CASE]["median_mib_per_second"])
    minimum_throughput = baseline_throughput * (1.0 - threshold / 100.0)
    passed = current_throughput >= minimum_throughput

    # Unlike the absolute gate above, these hold on any host, so they are
    # checked whether or not the environment matches.
    observed = speedups(points)
    blocking = {name: gates for name, _, _, gates in SPEEDUP_FLOORS}
    floors = baseline.get("speedup_floors", {}).get("cases", {})
    speedup_report = {}
    speedups_passed = True
    for name, value in observed.items():
        floor = floors.get(name, {}).get("floor")
        held = floor is None or value >= float(floor)
        if blocking[name]:
            speedups_passed &= held
        speedup_report[name] = {
            "measured": round(value, 3),
            "floor": floor,
            "blocking": blocking[name],
            "passed": held,
        }

    status = "pass" if (passed or not GATE_BLOCKING) else "fail"
    if not comparable:
        status = "non_comparable"
    if not speedups_passed:
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
            "cases": speedup_report,
        },
        "points": points,
        "skew_points": skew_points,
        "skew": skew,
        "skew_baseline": baseline.get("skew"),
        **attribution,
    }
    emit(result, args.output)
    if not speedups_passed:
        return 1
    return 1 if GATE_BLOCKING and comparable and not passed else 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except subprocess.CalledProcessError as error:
        if error.stdout:
            print(error.stdout, file=sys.stderr)
        if error.stderr:
            print(error.stderr, file=sys.stderr)
        raise SystemExit(error.returncode) from error
