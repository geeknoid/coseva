#!/usr/bin/env python3
"""Run repeated paired file-backed encoding measurements against csv.

Each Criterion target case executes both implementations in alternating order
inside every custom iteration, reports only the target implementation to
Criterion, and persists both adjacent elapsed totals for ratio calculation.
The runner takes the worse paired ratio from the two target cases, repeats the
whole Criterion measurement at least three times, and gates on the minimum
independent-run ratio. Paired work doubles each iteration and three independent
runs make the scheduled default roughly six times as expensive as the former
unpaired single-run methodology.

The path/oversized target-case observations are inconsistent, so their
recomputed minimum is retained conservatively for reporting but never gates the
writer/typical P2 completion criterion.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import platform
import statistics
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
BENCHMARK_FILE = Path("crates/coseva/benches/encode_wallclock.rs")
DEFAULT_BASELINE = HARNESS_DIR / "baseline.json"
GROUP = "encode_wallclock"
FEATURES = "std,derive,serde"
SHAPES = ("typical", "oversized")
APIS = ("writer", "path")
METRIC = "paired_criterion_median_target_throughput_mib_per_second"
SCHEMA = 2
MIN_RUNS = 3
RATIO_MARGIN_PERCENT = 20.0
ABSOLUTE_THRESHOLD_PERCENT = 10.0
RATIO_TOLERANCE = 1e-12
REPORT_ONLY_RATIO_KEYS = frozenset({"path/oversized"})


def point_id(implementation: str, api: str, shape: str) -> str:
    return f"{GROUP}/{implementation}/{api}/{shape}"


def required_ids() -> list[str]:
    return [
        point_id(implementation, api, shape)
        for shape in SHAPES
        for api in APIS
        for implementation in ("coseva", "csv")
    ]


def paired_ratios(
    pairs: dict[str, Any],
) -> tuple[dict[str, float], dict[str, list[dict[str, Any]]]]:
    evidence = {}
    result = {}
    for shape in SHAPES:
        for api in APIS:
            key = f"{api}/{shape}"
            observations = []
            for target in ("coseva", "csv"):
                point = pairs[point_id(target, api, shape)]
                coseva_ns = int(point["coseva_ns"])
                csv_ns = int(point["csv_ns"])
                executions = int(point["executions"])
                if coseva_ns <= 0 or csv_ns <= 0 or executions <= 0:
                    raise SystemExit(f"invalid paired timing totals for {key}/{target}")
                observations.append(
                    {
                        "target_case": target,
                        "coseva_ns": coseva_ns,
                        "csv_ns": csv_ns,
                        "executions": executions,
                        "ratio": csv_ns / coseva_ns,
                    }
                )
            evidence[key] = observations
            result[key] = min(item["ratio"] for item in observations)
    return result, evidence


def collect_pairs(target_dir: Path) -> dict[str, Any]:
    path = target_dir / "encode-pairs.json"
    pairs = json.loads(path.read_text(encoding="utf-8"))
    missing = sorted(set(required_ids()) - set(pairs))
    if missing:
        raise SystemExit("paired timing artifact is incomplete: " + ", ".join(missing))
    return pairs


def point_ratios(points: dict[str, Any]) -> dict[str, float]:
    return {
        f"{api}/{shape}": float(points[point_id("csv", api, shape)]["median_ns"])
        / float(points[point_id("coseva", api, shape)]["median_ns"])
        for shape in SHAPES
        for api in APIS
    }


def conservative_ratios(run_ratios: list[dict[str, float]]) -> dict[str, float]:
    return {
        key: min(run[key] for run in run_ratios)
        for key in run_ratios[0]
    }


def ratio_baseline(key: str, observations: list[float]) -> dict[str, Any]:
    measured = [round(value, 4) for value in observations]
    conservative = min(observations)
    if conservative < 1.0 or key in REPORT_ONLY_RATIO_KEYS:
        return {
            "observations": measured,
            "conservative": round(conservative, 4),
            "conservative_statistic": "minimum",
            "floor": None,
            "blocking": False,
            "status": (
                "report_only_below_parity"
                if conservative < 1.0
                else "report_only_outside_completion_criterion"
            ),
        }
    return {
        "observations": measured,
        "conservative": round(conservative, 4),
        "conservative_statistic": "minimum",
        "floor": max(
            1.0,
            round(conservative * (1.0 - RATIO_MARGIN_PERCENT / 100.0), 4),
        ),
        "blocking": True,
        "status": "blocking_at_or_above_parity",
    }


def is_finite_number(value: Any) -> bool:
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        return False
    try:
        return math.isfinite(float(value))
    except OverflowError:
        return False


def ratio_matches(stored: Any, recomputed: float) -> bool:
    return is_finite_number(stored) and math.isclose(
        float(stored),
        recomputed,
        rel_tol=RATIO_TOLERANCE,
        abs_tol=RATIO_TOLERANCE,
    )


def validate_baseline(baseline: dict[str, Any], path: Path) -> None:
    if baseline.get("schema") != SCHEMA or baseline.get("metric") != METRIC:
        raise SystemExit(
            f"unsupported baseline schema or metric in {path}; schema {SCHEMA} "
            "requires repeated paired observations and rejects old "
            "single-observation blocking data"
        )
    independent_runs = baseline.get("independent_runs")
    runs = baseline.get("runs")
    if (
        not isinstance(independent_runs, int)
        or independent_runs < MIN_RUNS
        or not isinstance(runs, list)
        or len(runs) != independent_runs
    ):
        raise SystemExit(
            f"{path} must preserve at least {MIN_RUNS} independent paired runs"
        )
    if baseline.get("source", {}).get("runtime_tree_clean") is not True:
        raise SystemExit(
            f"{path} was not recorded from a clean runtime source tree; "
            "regenerate it from a clean committed snapshot"
        )
    expected_ratio_keys = {
        f"{api}/{shape}" for shape in SHAPES for api in APIS
    }
    expected_point_ids = set(required_ids())
    recomputed_run_ratios = {key: [] for key in expected_ratio_keys}
    for expected_index, run in enumerate(runs, start=1):
        if (
            not isinstance(run, dict)
            or run.get("index") != expected_index
            or not isinstance(run.get("points"), dict)
            or set(run.get("points", {})) != expected_point_ids
            or not isinstance(run.get("ratios"), dict)
            or set(run.get("ratios", {})) != expected_ratio_keys
            or not isinstance(run.get("paired_evidence"), dict)
            or set(run.get("paired_evidence", {})) != expected_ratio_keys
        ):
            raise SystemExit(
                f"{path} has incomplete evidence for independent run "
                f"{expected_index}"
            )
        for key in expected_ratio_keys:
            evidence = run["paired_evidence"][key]
            if (
                not isinstance(evidence, list)
                or len(evidence) != 2
                or not all(isinstance(item, dict) for item in evidence)
                or {item.get("target_case") for item in evidence}
                != {"coseva", "csv"}
                or any(
                    not is_finite_number(item.get("coseva_ns"))
                    or float(item["coseva_ns"]) <= 0
                    or not is_finite_number(item.get("csv_ns"))
                    or float(item["csv_ns"]) <= 0
                    or not isinstance(item.get("executions"), int)
                    or isinstance(item.get("executions"), bool)
                    or item["executions"] <= 0
                    for item in evidence
                )
            ):
                raise SystemExit(
                    f"{path} has invalid paired evidence for run "
                    f"{expected_index}, {key}"
                )
            target_ratios = [
                float(item["csv_ns"]) / float(item["coseva_ns"])
                for item in evidence
            ]
            for item, recomputed in zip(evidence, target_ratios, strict=True):
                if not ratio_matches(item.get("ratio"), recomputed):
                    raise SystemExit(
                        f"{path} has a stored observation ratio that does not "
                        f"match elapsed evidence for run {expected_index}, "
                        f"{key}/{item['target_case']}"
                    )
            paired_minimum = min(target_ratios)
            if not ratio_matches(run["ratios"][key], paired_minimum):
                raise SystemExit(
                    f"{path} does not use the worse paired ratio for run "
                    f"{expected_index}, {key}"
                )
            recomputed_run_ratios[key].append(paired_minimum)
    ratio_floors = baseline.get("ratio_floors")
    if (
        not isinstance(ratio_floors, dict)
        or set(ratio_floors) != expected_ratio_keys
    ):
        raise SystemExit(f"{path} has incomplete ratio-floor evidence")
    for key in expected_ratio_keys:
        case = baseline.get("ratio_floors", {}).get(key, {})
        expected = ratio_baseline(key, recomputed_run_ratios[key])
        if not isinstance(case, dict):
            raise SystemExit(
                f"{path} has invalid repeated ratio evidence for {key}"
            )
        observations = case.get("observations")
        if (
            not isinstance(observations, list)
            or len(observations) != independent_runs
            or not all(is_finite_number(value) for value in observations)
            or any(
                not ratio_matches(stored, recomputed)
                for stored, recomputed in zip(
                    observations, expected["observations"], strict=True
                )
            )
        ):
            raise SystemExit(
                f"{path} ratio observations do not match recomputed runs for {key}"
            )
        if (
            case.get("conservative_statistic")
            != expected["conservative_statistic"]
            or not ratio_matches(
                case.get("conservative"), expected["conservative"]
            )
            or case.get("blocking") is not expected["blocking"]
            or case.get("status") != expected["status"]
            or (
                expected["floor"] is None
                and case.get("floor") is not None
            )
            or (
                expected["floor"] is not None
                and not ratio_matches(case.get("floor"), expected["floor"])
            )
        ):
            raise SystemExit(
                f"{path} has ratio-floor policy that does not match "
                f"recomputed runs for {key}"
            )


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(
        description=__doc__,
        epilog=(
            "Only within-run coseva/csv ratios block. Absolute throughput is "
            "report-only and comparable solely on the baseline environment. "
            "The default performs three independent paired runs."
        ),
    )
    result.add_argument("--baseline", type=Path, default=DEFAULT_BASELINE)
    result.add_argument("--output", type=Path)
    result.add_argument("--samples", type=int, default=20)
    result.add_argument("--warm-up-time", type=float, default=1.0)
    result.add_argument("--measurement-time", type=float, default=5.0)
    result.add_argument(
        "--runs",
        type=int,
        default=MIN_RUNS,
        help="independent paired Criterion runs (minimum and default: 3)",
    )
    result.add_argument(
        "--host-id",
        default=os.environ.get("COSEVA_ENCODE_BENCH_HOST", platform.node()),
    )
    result.add_argument("--source-root", type=Path, default=REPOSITORY)
    result.add_argument(
        "--target-dir", type=Path, default=REPOSITORY / "target/encode-bench"
    )
    result.add_argument("--allow-dirty", action="store_true")
    result.add_argument("--skip-run", action="store_true")
    result.add_argument("--write-baseline", action="store_true")
    return result


def main() -> int:
    args = parser().parse_args()
    if args.samples < 10:
        raise SystemExit("--samples must be at least 10 (Criterion requirement)")
    if args.runs < MIN_RUNS:
        raise SystemExit(f"--runs must be at least {MIN_RUNS}")
    if args.skip_run:
        raise SystemExit(
            "--skip-run cannot supply independent paired observations; run "
            "the benchmark or validate the schema through validate_baseline"
        )
    source_root = args.source_root.resolve()
    target_dir = args.target_dir.resolve()
    revision = git(source_root, "rev-parse", "HEAD")
    dirty = dirty_paths(source_root)
    runtime_dirty = dirty
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
        validate_baseline(baseline, args.baseline)
        if baseline["benchmark"]["fingerprint"] != fingerprint:
            raise SystemExit(
                "encode benchmark differs from the explicit baseline; "
                "refresh it on the pinned environment"
            )
        comparable = baseline["environment_key"] == env_key

    run_results = []
    expected_oracles: dict[str, Any] | None = None
    for index in range(1, args.runs + 1):
        measure(
            source_root,
            target_dir,
            bench="encode_wallclock",
            groups=(GROUP,),
            features=FEATURES,
            warm_up=args.warm_up_time,
            measurement=args.measurement_time,
            samples=args.samples,
        )
        points = collect(target_dir, GROUP, required_ids())
        pairs = collect_pairs(target_dir)
        observed, pair_evidence = paired_ratios(pairs)
        oracles = json.loads(
            (target_dir / "encode-oracles.json").read_text(encoding="utf-8")
        )
        if expected_oracles is None:
            expected_oracles = oracles
        elif oracles != expected_oracles:
            raise SystemExit(
                f"write oracles changed between independent runs 1 and {index}"
            )
        run_results.append(
            {
                "index": index,
                "points": points,
                "point_ratios": point_ratios(points),
                "paired_evidence": pair_evidence,
                "ratios": observed,
            }
        )

    assert expected_oracles is not None
    run_ratios = [run["ratios"] for run in run_results]
    observed = conservative_ratios(run_ratios)
    observations = {
        key: [run[key] for run in run_ratios]
        for key in observed
    }
    points = {
        key: {
            "bytes": run_results[0]["points"][key]["bytes"],
            "median_ns": round(
                statistics.median(
                    float(run["points"][key]["median_ns"])
                    for run in run_results
                ),
                3,
            ),
            "median_mib_per_second": round(
                statistics.median(
                    float(run["points"][key]["median_mib_per_second"])
                    for run in run_results
                ),
                3,
            ),
        }
        for key in required_ids()
    }

    if args.write_baseline:
        result = {
            "schema": SCHEMA,
            "metric": METRIC,
            "samples": args.samples,
            "warm_up_seconds": args.warm_up_time,
            "measurement_seconds": args.measurement_time,
            "independent_runs": args.runs,
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
                "buffer_bytes": 8192,
                "page_cache_semantics": (
                    "files are truncated per sample and not sync_all'd; timings "
                    "include writes into the operating system page cache, not durability"
                ),
                "methodology": (
                    "each reported target executes both implementations in "
                    "alternating order per iter_custom iteration, returns only "
                    "target elapsed time to Criterion, and persists both "
                    "adjacent elapsed totals; each run uses the worse ratio "
                    "from its two target cases and the gate uses the minimum "
                    "across independent runs"
                ),
                "runtime_cost": (
                    "paired execution doubles work per iteration; three "
                    "independent runs cost roughly six unpaired runs"
                ),
                "report_only_cases": {
                    "path/oversized": (
                        "target-case observations are inconsistent; preserve "
                        "their recomputed minimum conservatively for reporting "
                        "without using it to gate the writer/typical P2 criterion"
                    )
                },
            },
            "ratio_floors": {
                key: ratio_baseline(key, observations[key])
                for key in observed
            },
            "absolute_threshold_percent": ABSOLUTE_THRESHOLD_PERCENT,
            "points": points,
            "runs": run_results,
            "write_oracles": expected_oracles,
        }
        args.baseline.parent.mkdir(parents=True, exist_ok=True)
        args.baseline.write_text(
            json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        print(f"wrote {args.baseline}", file=sys.stderr)
        return 0

    assert baseline is not None
    ratio_report = {}
    ratios_passed = True
    for key, value in observed.items():
        baseline_case = baseline["ratio_floors"][key]
        blocking = bool(baseline_case["blocking"])
        floor = baseline_case["floor"]
        passed = value >= float(floor) if blocking else None
        if blocking:
            ratios_passed &= bool(passed)
        ratio_report[key] = {
            "observations": [
                round(observation, 4)
                for observation in observations[key]
            ],
            "conservative": round(value, 4),
            "conservative_statistic": "minimum",
            "floor": floor,
            "passed": passed,
            "blocking": blocking,
            "status": (
                "pass" if passed else "fail"
                if blocking
                else "report_only_below_parity"
                if value < 1.0
                else "report_only_at_or_above_parity"
            ),
        }

    absolute_report = {}
    threshold = float(baseline["absolute_threshold_percent"])
    for key, point in points.items():
        old = float(baseline["points"][key]["median_mib_per_second"])
        current = float(point["median_mib_per_second"])
        absolute_report[key] = {
            "baseline_mib_per_second": old,
            "current_mib_per_second": current,
            "regression_percent": round((1.0 - current / old) * 100.0, 3),
            "within_report_band": current >= old * (1.0 - threshold / 100.0),
            "blocking": False,
            "comparable": comparable,
            "run_mib_per_second": [
                run["points"][key]["median_mib_per_second"]
                for run in run_results
            ],
        }

    result = {
        "schema": SCHEMA,
        "metric": METRIC,
        "status": "pass" if ratios_passed else "fail",
        "comparable": comparable,
        "canonical": comparable and not runtime_dirty,
        "independent_runs": args.runs,
        "generated_at": datetime.now(UTC).isoformat(),
        "environment_key": env_key,
        "baseline_environment_key": baseline["environment_key"],
        "host": host,
        "toolchain": toolchain,
        "source": {
            "revision": revision,
            "runtime_tree_clean": not runtime_dirty,
            "dirty_entry_count": len(dirty),
        },
        "ratio_floors": ratio_report,
        "absolute_throughput": absolute_report,
        "points": points,
        "runs": run_results,
        "write_oracles": expected_oracles,
    }
    emit(result, args.output)
    return 0 if ratios_passed else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except subprocess.CalledProcessError as error:
        sys.stderr.write(error.stderr or "")
        raise SystemExit(error.returncode) from error
