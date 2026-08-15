#!/usr/bin/env python3
"""Measure downstream CsvDecode/CsvEncode derive compile wall time."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import platform
import shutil
import statistics
import subprocess
import sys
import time
from datetime import UTC, datetime
from pathlib import Path
from typing import Any


HARNESS_DIR = Path(__file__).resolve().parent
REPOSITORY = HARNESS_DIR.parents[3]
DEFAULT_BASELINE = HARNESS_DIR / "baseline.json"
LOCKFILE = HARNESS_DIR / "fixture.Cargo.lock"
PACKAGE = "coseva-proc-macro-bench-fixture"
CASES = (
    ("narrow_decode", "decode", 8),
    ("narrow_encode", "encode", 8),
    ("wide_decode", "decode", 128),
    ("wide_encode", "encode", 128),
    ("attribute_generic_decode", "attribute_generic_decode", 0),
)
METRIC = "20pct_trimmed_mean_downstream_clean_check_wall_ms"
SCHEMA = 1


def run(
    command: list[str],
    *,
    cwd: Path,
    env: dict[str, str] | None = None,
    capture: bool = True,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        command,
        cwd=cwd,
        env=env,
        check=True,
        text=True,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.PIPE if capture else None,
    )


def git(repository: Path, *args: str) -> str:
    return run(["git", "-C", str(repository), *args], cwd=repository).stdout.strip()


def cpu_model() -> str:
    cpuinfo = Path("/proc/cpuinfo")
    if cpuinfo.exists():
        for line in cpuinfo.read_text(encoding="utf-8").splitlines():
            if line.lower().startswith("model name"):
                return line.split(":", 1)[1].strip()
    return platform.processor() or "unknown"


def parse_rustc() -> dict[str, str]:
    lines = run(["rustc", "-Vv"], cwd=REPOSITORY).stdout.splitlines()
    values: dict[str, str] = {"version": lines[0]}
    for line in lines[1:]:
        if ": " in line:
            key, value = line.split(": ", 1)
            values[key.replace("-", "_")] = value
    values["cargo"] = run(["cargo", "-V"], cwd=REPOSITORY).stdout.strip()
    return values


def host_metadata(host_id: str, cpu: int | None) -> dict[str, Any]:
    affinity = (
        sorted(os.sched_getaffinity(0))
        if hasattr(os, "sched_getaffinity")
        else list(range(os.cpu_count() or 1))
    )
    return {
        "id": host_id,
        "hostname": platform.node(),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "cpu_model": cpu_model(),
        "logical_cpus": os.cpu_count(),
        "affinity": [cpu] if cpu is not None else affinity,
    }


def environment_key(host: dict[str, Any], toolchain: dict[str, str]) -> str:
    stable_identity = {
        "host_id": host["id"],
        "machine": host["machine"],
        "cpu_model": host["cpu_model"],
        "affinity": host["affinity"],
        "rustc_release": toolchain.get("release"),
        "rustc_commit_hash": toolchain.get("commit_hash"),
        "rustc_host": toolchain.get("host"),
        "cargo": toolchain["cargo"],
    }
    encoded = json.dumps(stable_identity, sort_keys=True).encode()
    return hashlib.sha256(encoded).hexdigest()


def decode_source(width: int) -> str:
    fields = []
    for index in range(width):
        attrs = []
        if index % 16 == 0:
            attrs.append(
                f'#[csv(rename = "column-{index}", alias = "legacy_{index}", '
                f'alias = "old_{index}")]'
            )
        elif index % 16 == 1:
            attrs.append("#[csv(default)]")
        fields.extend([f"    {attr}" for attr in attrs])
        fields.append(f"    field_{index:03}: u64,")
    return "\n".join(
        [
            "#![allow(dead_code)]",
            "",
            "use coseva::encoding::CsvDecode;",
            "",
            "#[derive(CsvDecode)]",
            '#[csv(rename_all = "kebab-case")]',
            "struct Row {",
            *fields,
            "}",
            "",
            "fn main() {",
            "    let _ = <Row as CsvDecode>::field_names();",
            "}",
            "",
        ]
    )


def encode_source(width: int) -> str:
    fields = []
    for index in range(width):
        if index % 16 == 0:
            fields.append('    #[csv(format_with = "format_u64")]')
        elif index % 16 == 1:
            fields.append(f'    #[csv(rename = "column-{index}")]')
        fields.append(f"    field_{index:03}: u64,")
    return "\n".join(
        [
            "#![allow(dead_code)]",
            "",
            "use coseva::encoding::CsvEncode;",
            "",
            "fn format_u64(value: &u64) -> String {",
            "    value.to_string()",
            "}",
            "",
            "#[derive(CsvEncode)]",
            '#[csv(rename_all = "SCREAMING_SNAKE_CASE")]',
            "struct Row {",
            *fields,
            "}",
            "",
            "fn main() {",
            "    let _ = <Row as CsvEncode>::field_names();",
            "}",
            "",
        ]
    )


def attribute_generic_decode_source() -> str:
    return """#![allow(dead_code)]

use std::marker::PhantomData;

use coseva::encoding::CsvDecode;

fn parse_decimal(bytes: &[u8]) -> Result<u64, std::num::ParseIntError> {
    std::str::from_utf8(bytes).unwrap_or("0").parse()
}

#[derive(CsvDecode)]
#[csv(rename_all = "kebab-case")]
struct AttributeGenericRow<'row, T, U, const N: usize>
where
    T: Default,
    U: Default,
    [u8; N]: Default,
{
    #[csv(rename = "identifier", alias = "id", alias = "legacy_id")]
    identifier: &'row str,
    raw_value: &'row [u8],
    #[csv(parse_with = "parse_decimal")]
    item_count: u64,
    #[csv(parse_with = "parse_decimal")]
    total_count: u64,
    #[csv(default)]
    optional_count: u32,
    #[csv(skip)]
    state: T,
    #[csv(skip)]
    marker: PhantomData<U>,
    #[csv(skip)]
    padding: [u8; N],
}

fn main() {
    type Row = AttributeGenericRow<'static, String, Vec<u8>, 8>;
    let _ = <Row as CsvDecode>::field_names();
}
"""


def fixture_sources(source_root: Path) -> dict[str, str]:
    bins = "\n".join(
        f"""
[[bin]]
name = "{name}"
path = "src/bin/{name}.rs"
"""
        for name, _, _ in CASES
    )
    manifest = f"""[package]
name = "{PACKAGE}"
version = "0.0.0"
edition = "2024"
rust-version = "1.95"
publish = false
autobins = false

[workspace]

[dependencies]
coseva = {{ path = {json.dumps(str(source_root / "crates/coseva"))}, default-features = false, features = ["std", "derive"] }}
{bins}
"""
    sources = {"Cargo.toml": manifest}
    for name, derive, width in CASES:
        if derive == "decode":
            source = decode_source(width)
        elif derive == "encode":
            source = encode_source(width)
        else:
            source = attribute_generic_decode_source()
        sources[f"src/bin/{name}.rs"] = source
    return sources


def fixture_fingerprint(
    sources: dict[str, str], lockfile: str, source_root: Path
) -> str:
    digest = hashlib.sha256()
    for path, contents in sorted(sources.items()):
        if path == "Cargo.toml":
            contents = contents.replace(
                json.dumps(str(source_root / "crates/coseva")),
                '"<SOURCE_ROOT>/crates/coseva"',
            )
        digest.update(path.encode())
        digest.update(b"\0")
        digest.update(contents.encode())
        digest.update(b"\0")
    digest.update(lockfile.encode())
    return digest.hexdigest()


def prepare_fixture(source_root: Path, work_dir: Path) -> tuple[Path, str]:
    fixture = work_dir / "fixture"
    if fixture.exists():
        shutil.rmtree(fixture)
    fixture.mkdir(parents=True)
    sources = fixture_sources(source_root)
    for relative, contents in sources.items():
        path = fixture / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents, encoding="utf-8")
    lockfile = LOCKFILE.read_text(encoding="utf-8")
    (fixture / "Cargo.lock").write_text(lockfile, encoding="utf-8")
    return fixture, fixture_fingerprint(sources, lockfile, source_root)


def cargo_command(cpu: int | None, *args: str) -> list[str]:
    command = ["cargo", *args]
    if cpu is not None:
        command = ["taskset", "--cpu-list", str(cpu), *command]
    return command


class Arm:
    """One fixture tree under measurement, with its own Cargo target directory."""

    def __init__(self, name: str, fixture: Path, work_dir: Path) -> None:
        self.name = name
        self.manifest = fixture / "Cargo.toml"
        self.fixture = fixture
        self.env = os.environ.copy()
        self.env.update(
            {
                "CARGO_INCREMENTAL": "0",
                "CARGO_TARGET_DIR": str(work_dir / "target"),
                "CARGO_TERM_COLOR": "never",
            }
        )

    def cargo(self, cpu: int | None, *args: str) -> None:
        run(
            cargo_command(cpu, *args, "--quiet", "--manifest-path", str(self.manifest)),
            cwd=self.fixture,
            env=self.env,
        )

    def prepare(self, cpu: int | None) -> float:
        start = time.perf_counter()
        self.cargo(cpu, "check", "--locked", "--bins")
        return (time.perf_counter() - start) * 1000

    def sample(self, cpu: int | None, case: str) -> float:
        self.cargo(cpu, "clean", "-p", PACKAGE)
        start = time.perf_counter()
        self.cargo(cpu, "check", "--locked", "--bin", case)
        return (time.perf_counter() - start) * 1000


def measure(
    arms: list[Arm],
    samples: int,
    cpu: int | None,
) -> tuple[dict[str, float], dict[str, dict[str, list[float]]]]:
    preparation_ms = {arm.name: round(arm.prepare(cpu), 3) for arm in arms}
    measurements = {arm.name: {name: [] for name, _, _ in CASES} for arm in arms}
    case_names = [name for name, _, _ in CASES]
    for round_index in range(samples):
        rotation = round_index % len(case_names)
        ordered = case_names[rotation:] + case_names[:rotation]
        # Alternate which arm goes first so a drifting machine biases neither.
        arm_order = arms if round_index % 2 == 0 else list(reversed(arms))
        for name in ordered:
            for arm in arm_order:
                elapsed_ms = arm.sample(cpu, name)
                measurements[arm.name][name].append(round(elapsed_ms, 3))
                print(
                    f"{arm.name} {name} sample {round_index + 1}/{samples}: {elapsed_ms:.1f} ms",
                    file=sys.stderr,
                )
    return preparation_ms, measurements


def stable_wall_ms(values: list[float]) -> float:
    ordered = sorted(values)
    trim = max(1, len(ordered) // 5)
    return round(statistics.fmean(ordered[trim:-trim]), 3)


def _kept(values: list[float]) -> list[float]:
    ordered = sorted(values)
    trim = max(1, len(ordered) // 5)
    return ordered[trim:-trim]


def spread_percent(values: list[float]) -> float:
    """Half the trimmed range, relative to the trimmed mean, in percent.

    Descriptive only: it reports how jittery the individual samples were, not
    how uncertain their mean is.
    """
    kept = _kept(values)
    return round((kept[-1] - kept[0]) / 2.0 / statistics.fmean(kept) * 100.0, 3)


def standard_error_percent(values: list[float]) -> float:
    """Standard error of the trimmed mean, relative to it, in percent.

    This — not the sample spread — is what a regression threshold must clear.
    Individual clean-check samples on a shared machine scatter by tens of
    percent while their trimmed mean stays stable to a few, which is the whole
    reason the metric is a trimmed mean.
    """
    kept = _kept(values)
    if len(kept) < 2:
        return 0.0
    error = statistics.stdev(kept) / math.sqrt(len(kept))
    return round(error / statistics.fmean(kept) * 100.0, 3)


def emit(result: dict[str, Any], output: Path | None) -> None:
    encoded = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if output:
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(encoded, encoding="utf-8")
    sys.stdout.write(encoded)


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(
        description=__doc__,
        epilog=(
            "CI: pin the baseline toolchain and named host, run this command on a clean "
            "checkout, and archive --output JSON. Refresh deliberately with "
            "--write-baseline on that same clean host."
        ),
    )
    result.add_argument(
        "--baseline",
        type=Path,
        default=DEFAULT_BASELINE,
        help="explicit JSON baseline (default: %(default)s)",
    )
    result.add_argument("--output", type=Path, help="also write the JSON result to this path")
    result.add_argument("--samples", type=int, help="samples per case; minimum 3")
    result.add_argument(
        "--host-id",
        default=os.environ.get("COSEVA_MACRO_BENCH_HOST", platform.node()),
        help="stable host name, or COSEVA_MACRO_BENCH_HOST (default: %(default)s)",
    )
    result.add_argument("--cpu", type=int, help="pin Cargo/rustc to this CPU with taskset")
    result.add_argument(
        "--source-root",
        type=Path,
        default=REPOSITORY,
        help="clean Git checkout to benchmark (default: repository root)",
    )
    result.add_argument(
        "--work-dir",
        type=Path,
        default=REPOSITORY / "target/proc-macro-bench",
        help="generated fixture and Cargo target directory",
    )
    result.add_argument(
        "--allow-dirty",
        action="store_true",
        help="allow a non-canonical diagnostic run; baseline writes remain forbidden",
    )
    result.add_argument(
        "--ignore-environment",
        action="store_true",
        help="run diagnostics elsewhere, but keep the result non-comparable",
    )
    result.add_argument(
        "--against",
        type=Path,
        help=(
            "second checkout to compare --source-root against, interleaved sample "
            "for sample; host-portable, so this is the mode CI runs"
        ),
    )
    result.add_argument(
        "--threshold-percent",
        type=float,
        help="regression margin; defaults to the baseline's for --baseline runs and to 25 for --against",
    )
    result.add_argument(
        "--require-comparable",
        action="store_true",
        help="fail instead of passing silently when the environment does not match the baseline",
    )
    result.add_argument(
        "--write-baseline",
        action="store_true",
        help="replace --baseline from a clean checkout instead of checking regressions",
    )
    return result


def compare(args: argparse.Namespace, cpu: int | None, samples: int) -> int:
    """A/B two checkouts on one machine, which needs no committed absolute baseline."""
    head_root = args.source_root.resolve()
    base_root = args.against.resolve()
    work_dir = args.work_dir.resolve()
    threshold = 25.0 if args.threshold_percent is None else args.threshold_percent

    arms = []
    fingerprints = {}
    for name, root in (("base", base_root), ("head", head_root)):
        fixture, fingerprint = prepare_fixture(root, work_dir / name)
        fingerprints[name] = fingerprint
        arms.append(Arm(name, fixture, work_dir / name))

    preparation_ms, measurements = measure(arms, samples, cpu)

    cases = {}
    failed = False
    for name, _, _ in CASES:
        base_ms = stable_wall_ms(measurements["base"][name])
        head_ms = stable_wall_ms(measurements["head"][name])
        regression = (head_ms / base_ms - 1.0) * 100.0
        passed = regression <= threshold
        failed |= not passed
        cases[name] = {
            "base_ms": base_ms,
            "head_ms": head_ms,
            "base_spread_percent": spread_percent(measurements["base"][name]),
            "head_spread_percent": spread_percent(measurements["head"][name]),
            "base_standard_error_percent": standard_error_percent(measurements["base"][name]),
            "head_standard_error_percent": standard_error_percent(measurements["head"][name]),
            "regression_percent": round(regression, 3),
            "passed": passed,
        }

    emit(
        {
            "schema": SCHEMA,
            "metric": METRIC,
            "mode": "against",
            "status": "fail" if failed else "pass",
            "threshold_percent": threshold,
            "generated_at": datetime.now(UTC).isoformat(),
            "host": host_metadata(args.host_id, cpu),
            "toolchain": parse_rustc(),
            "samples": samples,
            "fixture": {"fingerprints": fingerprints},
            "source": {
                "head": git(head_root, "rev-parse", "HEAD"),
                "base": git(base_root, "rev-parse", "HEAD"),
            },
            "preparation_wall_ms": preparation_ms,
            "cases": cases,
        },
        args.output,
    )
    return 1 if failed else 0


def main() -> int:
    args = parser().parse_args()
    if args.samples is not None and args.samples < 3:
        raise SystemExit("--samples must be at least 3")
    source_root = args.source_root.resolve()
    work_dir = args.work_dir.resolve()
    revision = git(source_root, "rev-parse", "HEAD")
    dirty_entries = git(source_root, "status", "--porcelain=v1", "--untracked-files=all").splitlines()
    tree_clean = not dirty_entries
    if not tree_clean and not args.allow_dirty:
        raise SystemExit(
            "source tree is dirty; commit/stash changes or use --allow-dirty for a non-canonical diagnostic run"
        )
    if args.write_baseline and not tree_clean:
        raise SystemExit("refusing to write a baseline from a dirty source tree")
    if args.against and args.write_baseline:
        raise SystemExit("--against compares two checkouts and writes no baseline")

    if args.against:
        cpu = args.cpu
        if cpu is not None and shutil.which("taskset") is None:
            raise SystemExit("the selected CPU requires taskset")
        return compare(args, cpu, args.samples or 11)

    baseline: dict[str, Any] | None = None
    if not args.write_baseline:
        baseline = json.loads(args.baseline.read_text(encoding="utf-8"))
        if baseline["schema"] != SCHEMA or baseline["metric"] != METRIC:
            raise SystemExit(f"unsupported baseline schema or metric in {args.baseline}")

    cpu = args.cpu
    if cpu is None and baseline and len(baseline["host"]["affinity"]) == 1:
        cpu = int(baseline["host"]["affinity"][0])
    if cpu is not None and shutil.which("taskset") is None:
        raise SystemExit("the selected baseline CPU requires taskset")

    samples = args.samples or (baseline["samples"] if baseline else 11)
    host = host_metadata(args.host_id, cpu)
    toolchain = parse_rustc()
    env_key = environment_key(host, toolchain)
    fixture, fingerprint = prepare_fixture(source_root, work_dir)

    if baseline and baseline["fixture"]["fingerprint"] != fingerprint:
        raise SystemExit(
            "fixture differs from the explicit baseline; create a clean reviewed baseline with --write-baseline"
        )
    comparable = not baseline or baseline["environment_key"] == env_key
    if baseline and not comparable and not args.ignore_environment:
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
        return 1 if args.require_comparable else 0

    preparation_wall, arm_measurements = measure(
        [Arm("head", fixture, work_dir)], samples, cpu
    )
    preparation_ms = preparation_wall["head"]
    measurements = arm_measurements["head"]
    statistics_ms = {
        name: stable_wall_ms(values)
        for name, values in measurements.items()
    }

    if args.write_baseline:
        # The margin has to clear the uncertainty in the statistic itself or it
        # fails on noise; four standard errors of the worst case, floored at 10%.
        worst_error = max(
            standard_error_percent(values) for values in measurements.values()
        )
        threshold_percent = (
            args.threshold_percent
            if args.threshold_percent is not None
            else max(10.0, round(4.0 * worst_error, 1))
        )
        result: dict[str, Any] = {
            "schema": SCHEMA,
            "metric": METRIC,
            "threshold_percent": threshold_percent,
            "threshold_basis": {
                "rule": "max(10.0, 4 * worst_case_standard_error_percent)",
                "worst_case_standard_error_percent": worst_error,
            },
            "samples": samples,
            "environment_key": env_key,
            "host": host,
            "toolchain": toolchain,
            "source": {"revision": revision, "tree_clean": True},
            "fixture": {
                "fingerprint": fingerprint,
                "widths": {"narrow": 8, "wide": 128},
                "attributes": [
                    "alias",
                    "rename",
                    "rename_all",
                    "default",
                    "skip",
                    "parse_with",
                    "format_with",
                ],
                "generic_fixture": {
                    "case": "attribute_generic_decode",
                    "lifetime_parameters": 1,
                    "type_parameters": 2,
                    "const_parameters": 1,
                },
            },
            "cases": {
                name: {
                    "baseline_ms": value,
                    "samples_ms": measurements[name],
                    "median_ms": round(statistics.median(measurements[name]), 3),
                    "spread_percent": spread_percent(measurements[name]),
                    "standard_error_percent": standard_error_percent(measurements[name]),
                }
                for name, value in statistics_ms.items()
            },
        }
        args.baseline.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(f"wrote {args.baseline}", file=sys.stderr)
        return 0

    assert baseline is not None
    threshold = (
        float(baseline["threshold_percent"])
        if args.threshold_percent is None
        else args.threshold_percent
    )
    cases = {}
    failed = False
    for name, _, _ in CASES:
        baseline_ms = float(baseline["cases"][name]["baseline_ms"])
        limit_ms = baseline_ms * (1.0 + threshold / 100.0)
        statistic_ms = statistics_ms[name]
        within_threshold = statistic_ms <= limit_ms
        failed |= not within_threshold
        cases[name] = {
            "samples_ms": measurements[name],
            "trimmed_mean_ms": statistic_ms,
            "median_ms": round(statistics.median(measurements[name]), 3),
            "baseline_ms": baseline_ms,
            "limit_ms": round(limit_ms, 3),
            "regression_percent": round((statistic_ms / baseline_ms - 1.0) * 100.0, 3),
            "passed": within_threshold if comparable else None,
        }

    canonical = tree_clean and comparable
    result = {
        "schema": SCHEMA,
        "metric": METRIC,
        "threshold_percent": threshold,
        "status": (
            "non_comparable"
            if not comparable
            else ("fail" if failed else "pass")
        ),
        "comparable": comparable,
        "canonical": canonical,
        "generated_at": datetime.now(UTC).isoformat(),
        "host": host,
        "toolchain": toolchain,
        "source": {
            "revision": revision,
            "tree_clean": tree_clean,
            "dirty_entry_count": len(dirty_entries),
        },
        "fixture": baseline["fixture"],
        "baseline": {
            "path": str(args.baseline),
            "revision": baseline["source"]["revision"],
            "environment_key": baseline["environment_key"],
        },
        "preparation_wall_ms": preparation_ms,
        "samples": samples,
        "cases": cases,
    }
    emit(result, args.output)
    return 1 if comparable and failed else 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except subprocess.CalledProcessError as error:
        if error.stdout:
            print(error.stdout, file=sys.stderr)
        if error.stderr:
            print(error.stderr, file=sys.stderr)
        raise SystemExit(error.returncode) from error
