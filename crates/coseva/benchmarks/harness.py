"""Shared plumbing for the wall-clock Criterion harnesses.

The parallel, index, and encode runners all have to answer the same questions
before a number means anything: which machine is this, which toolchain built it,
is the tree clean, and which Criterion artifacts came out. Those answers must
agree between the harnesses -- an environment key that differed between them
would silently compare a baseline against a host it was never taken on -- so
they are computed in exactly one place.

What stays in each harness is what differs: the cases it measures, the ratios it
derives, and the floors it gates.
"""

from __future__ import annotations

import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any


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


def parse_rustc(repository: Path) -> dict[str, str]:
    lines = run(["rustc", "-Vv"], cwd=repository).stdout.splitlines()
    values: dict[str, str] = {"version": lines[0]}
    for line in lines[1:]:
        if ": " in line:
            key, value = line.split(": ", 1)
            values[key.replace("-", "_")] = value
    values["cargo"] = run(["cargo", "-V"], cwd=repository).stdout.strip()
    return values


def host_metadata(host_id: str) -> dict[str, Any]:
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
        "affinity": affinity,
    }


def environment_key(host: dict[str, Any], toolchain: dict[str, str]) -> str:
    identity = {
        "host_id": host["id"],
        "platform": host["platform"],
        "machine": host["machine"],
        "cpu_model": host["cpu_model"],
        "logical_cpus": host["logical_cpus"],
        "affinity": host["affinity"],
        "rustc_release": toolchain.get("release"),
        "rustc_commit_hash": toolchain.get("commit_hash"),
        "rustc_host": toolchain.get("host"),
        "cargo": toolchain["cargo"],
    }
    return hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()


def benchmark_fingerprint(source_root: Path, benchmark_file: Path) -> str:
    return hashlib.sha256((source_root / benchmark_file).read_bytes()).hexdigest()


def dirty_paths(source_root: Path) -> list[str]:
    changed = git(source_root, "diff", "HEAD", "--name-only").splitlines()
    untracked = git(
        source_root, "ls-files", "--others", "--exclude-standard"
    ).splitlines()
    return sorted(set(changed + untracked))


def measure(
    source_root: Path,
    target_dir: Path,
    *,
    bench: str,
    groups: tuple[str, ...],
    features: str,
    warm_up: float,
    measurement: float,
    samples: int,
) -> None:
    """Re-measure `bench` from scratch into `target_dir`.

    Every group the benchmark defines must be listed: their previous artifacts
    are removed first so `collect` cannot read a stale estimate from a case the
    current benchmark no longer defines. A group left out of `groups` would keep
    reporting whatever it last measured, indefinitely.
    """
    for group in groups:
        criterion = target_dir / "criterion" / group
        if criterion.exists():
            shutil.rmtree(criterion)
    env = os.environ.copy()
    env.update(
        {
            "CARGO_INCREMENTAL": "0",
            "CARGO_TARGET_DIR": str(target_dir),
            "CARGO_TERM_COLOR": "never",
        }
    )
    command = [
        "cargo",
        "bench",
        "--manifest-path",
        str(source_root / "Cargo.toml"),
        "-p",
        "coseva",
        "--features",
        features,
        "--bench",
        bench,
        "--",
        "--noplot",
        "--warm-up-time",
        str(warm_up),
        "--measurement-time",
        str(measurement),
        "--sample-size",
        str(samples),
    ]
    run(command, cwd=source_root, env=env, capture=False)


def collect(
    target_dir: Path, group: str, required: list[str]
) -> dict[str, dict[str, float | int]]:
    """Read the median of every required case, or fail naming what is missing."""
    root = target_dir / "criterion" / group
    wanted = set(required)
    points: dict[str, dict[str, float | int]] = {}
    for benchmark_path in root.glob("**/new/benchmark.json"):
        benchmark = json.loads(benchmark_path.read_text(encoding="utf-8"))
        full_id = benchmark["full_id"]
        if full_id not in wanted:
            continue
        estimates = json.loads(
            benchmark_path.with_name("estimates.json").read_text(encoding="utf-8")
        )
        byte_count = int(benchmark["throughput"]["Bytes"])
        median_ns = float(estimates["median"]["point_estimate"])
        points[full_id] = {
            "bytes": byte_count,
            "median_ns": round(median_ns, 3),
            "median_mib_per_second": round(
                byte_count / (1 << 20) / (median_ns / 1_000_000_000), 3
            ),
        }
    missing = sorted(wanted - set(points))
    if missing:
        raise SystemExit(
            f"{group} Criterion artifacts are incomplete: " + ", ".join(missing)
        )
    return dict(sorted(points.items()))


def emit(result: dict[str, Any], output: Path | None) -> None:
    encoded = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if output:
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(encoded, encoding="utf-8")
    sys.stdout.write(encoded)
