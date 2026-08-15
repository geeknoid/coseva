#!/usr/bin/env python3
"""Produce the clean, machine-readable artifact consumed by perf_report.rs."""

from __future__ import annotations

import argparse
import json
import os
import platform
import subprocess
from pathlib import Path
from typing import Any


CRATE_ROOT = Path(__file__).resolve().parent.parent
WORKSPACE_ROOT = CRATE_ROOT.parent.parent
DEFAULT_OUTPUT = CRATE_ROOT / "docs/PERF.json"
DEFAULT_MACRO_ARTIFACT = (
    WORKSPACE_ROOT / "crates/coseva_macros/benchmarks/proc_macro/baseline.json"
)
SCHEMA = 1


def run(command: list[str], *, capture: bool = False) -> str:
    result = subprocess.run(
        command,
        cwd=WORKSPACE_ROOT,
        check=True,
        text=True,
        stdout=subprocess.PIPE if capture else None,
    )
    return result.stdout.strip() if capture else ""


def git(*arguments: str) -> str:
    return run(["git", *arguments], capture=True)


def cpu_model() -> str:
    cpuinfo = Path("/proc/cpuinfo")
    if cpuinfo.exists():
        for line in cpuinfo.read_text(encoding="utf-8").splitlines():
            if line.lower().startswith("model name"):
                return line.split(":", 1)[1].strip()
    return platform.processor() or "unknown"


def toolchain() -> dict[str, str]:
    lines = run(["rustc", "-Vv"], capture=True).splitlines()
    values = {"version": lines[0], "cargo": run(["cargo", "-V"], capture=True)}
    for line in lines[1:]:
        if ": " in line:
            key, value = line.split(": ", 1)
            values[key.replace("-", "_")] = value
    return values


def dispatch_arm() -> str:
    """The kernel arm the profiled process reached, not the one this host has.

    Every vector kernel is chosen by runtime CPU detection, and the benchmarks
    run under Valgrind, which emulates the guest CPU and answers `CPUID`
    itself — so `cpu_model` above describes the wrong machine for this purpose.
    `benches/dispatch.rs` reports the answer from inside the profiled binary;
    this only relays it, and says so when that benchmark has not run.
    """
    measured = WORKSPACE_ROOT / "target/perf-report/dispatch-arm.txt"
    if not measured.exists():
        return "unrecorded"
    return measured.read_text(encoding="utf-8").strip()


def evidence(command: str) -> dict[str, Any]:
    return {
        "command": command,
        "host": {
            "hostname": platform.node(),
            "platform": platform.platform(),
            "machine": platform.machine(),
            "cpu_model": cpu_model(),
            "logical_cpus": os.cpu_count(),
            "dispatch_arm": dispatch_arm(),
        },
        "toolchain": toolchain(),
        "source": {"revision": git("rev-parse", "HEAD"), "tree_clean": True},
    }


def callgrind_summary(path: Path) -> int:
    for line in path.read_text(encoding="utf-8").splitlines():
        if line.startswith("summary:"):
            return int(line.split()[1])
    raise ValueError(f"{path} has no summary line")


def collect_callgrind(bench: str, groups: tuple[str, ...]) -> list[dict[str, Any]]:
    root = WORKSPACE_ROOT / "target/gungraun/coseva" / bench
    results = []
    for group in groups:
        directory = root / group
        if not directory.is_dir():
            raise FileNotFoundError(f"missing benchmark artifacts under {directory}")
        for case_dir in sorted(path for path in directory.iterdir() if path.is_dir()):
            case, variant = case_dir.name.split(".", 1)
            output = case_dir / f"callgrind.{case}.{variant}.out"
            results.append(
                {
                    "case": case,
                    "variant": variant,
                    "instructions": callgrind_summary(output),
                }
            )
    return results


def collect_matrix() -> list[dict[str, Any]]:
    results = []
    roots = (
        (
            WORKSPACE_ROOT / "target/gungraun/coseva/matrix/matrix",
            lambda function: not function.startswith("byte_record_"),
        ),
        (
            WORKSPACE_ROOT
            / "target/gungraun/coseva/matrix_byte_record/matrix_byte_record",
            lambda function: function in {"byte_record_slice", "byte_record_push"},
        ),
        (
            WORKSPACE_ROOT
            / "target/gungraun/coseva/matrix_byte_record_io/matrix_byte_record_io",
            lambda function: function in {"byte_record_io", "byte_record_csv"},
        ),
    )
    for root, include in roots:
        if not root.is_dir():
            raise FileNotFoundError(f"missing matrix artifacts under {root}")
        for case_dir in sorted(path for path in root.iterdir() if path.is_dir()):
            function, document = case_dir.name.split(".", 1)
            if not include(function):
                continue
            output = case_dir / f"callgrind.{function}.{document}.out"
            results.append(
                {
                    "function": function,
                    "document": document,
                    "instructions": callgrind_summary(output),
                }
            )
    if not results:
        raise ValueError("no read-matrix artifacts found")
    return results


def collect_documents() -> list[dict[str, Any]]:
    command = [
        "cargo",
        "run",
        "--quiet",
        "--release",
        "-p",
        "coseva",
        "--features",
        "std,derive,serde",
        "--example",
        "document_dimensions",
    ]
    results = []
    for line in run(command, capture=True).splitlines():
        if not line or line.startswith("#"):
            continue
        name, byte_count, records, *_ = line.split("\t")
        results.append(
            {"name": name, "bytes": int(byte_count), "records": int(records)}
        )
    if not results:
        raise ValueError("document_dimensions produced no rows")
    return results


def collect_parallel() -> list[dict[str, Any]]:
    root = WORKSPACE_ROOT / "target/criterion/parallel"
    results = []
    for benchmark_path in sorted(root.glob("**/new/benchmark.json")):
        benchmark = json.loads(benchmark_path.read_text(encoding="utf-8"))
        full_id = benchmark["full_id"]
        if not (
            full_id.startswith("parallel/serial/")
            or full_id.startswith("parallel/fold/")
            or full_id.startswith("parallel/owned/")
        ):
            continue
        estimates_path = benchmark_path.with_name("estimates.json")
        estimates = json.loads(estimates_path.read_text(encoding="utf-8"))
        results.append(
            {
                "id": full_id,
                "bytes": int(benchmark["throughput"]["Bytes"]),
                "median_ns": float(estimates["median"]["point_estimate"]),
            }
        )
    required = [
        f"parallel/{path}/{size}MiB"
        for size in (8, 16, 32, 64)
        for path in (
            "serial/borrowed",
            "serial/owned",
            "fold/threads-auto",
            "owned/threads-auto",
        )
    ]
    required.extend(
        f"parallel/{path}/threads-{threads}/64MiB"
        for path in ("fold", "owned")
        for threads in ("2", "4", "8")
    )
    found = {point["id"] for point in results}
    missing = [name for name in required if name not in found]
    if missing:
        raise ValueError(f"parallel artifacts are incomplete: {', '.join(missing)}")
    return results


def macro_section(path: Path) -> dict[str, Any]:
    artifact = json.loads(path.read_text(encoding="utf-8"))
    source = artifact["source"]
    if not source.get("tree_clean"):
        raise ValueError(f"proc-macro artifact {path} was not measured from a clean tree")
    cases = []
    for name, values in sorted(artifact["cases"].items()):
        milliseconds = values.get("trimmed_mean_ms", values.get("baseline_ms"))
        if milliseconds is None:
            raise ValueError(f"proc-macro case {name} has no stable timing")
        cases.append({"case": name, "milliseconds": float(milliseconds)})
    command = artifact.get("command")
    if command is None:
        try:
            baseline_path = path.relative_to(WORKSPACE_ROOT)
        except ValueError:
            baseline_path = path
        parts = [
            "python3",
            "crates/coseva_macros/benchmarks/proc_macro/run.py",
            "--samples",
            str(artifact["samples"]),
            "--host-id",
            str(artifact["host"]["id"]),
        ]
        affinity = artifact["host"].get("affinity", [])
        if len(affinity) == 1:
            parts.extend(("--cpu", str(affinity[0])))
        parts.extend(("--write-baseline", "--baseline", str(baseline_path)))
        command = " ".join(parts)
    return {
        "evidence": {
            "command": command,
            "host": artifact["host"],
            "toolchain": artifact["toolchain"],
            "source": source,
        },
        "metric": artifact["metric"],
        "samples": artifact["samples"],
        "cases": cases,
    }


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    result.add_argument(
        "--macro-artifact", type=Path, default=DEFAULT_MACRO_ARTIFACT
    )
    return result


def main() -> int:
    args = parser().parse_args()
    dirty = git("status", "--porcelain=v1", "--untracked-files=all").splitlines()
    if dirty:
        raise SystemExit(
            "refusing to publish performance artifacts from a dirty tree; "
            "commit or stash every change first"
        )

    read_commands = (
        "cargo bench -p coseva --features std,derive,serde --bench matrix",
        "cargo bench -p coseva --features std,derive,serde --bench matrix_byte_record",
        "cargo bench -p coseva --features std,derive,serde --bench matrix_byte_record_io",
    )
    commands = {
        "read": " && ".join(read_commands),
        "write": "cargo bench -p coseva --features std,derive,serde --bench encode",
        "index": "cargo bench -p coseva --features std,index,derive --bench index",
        "parallel": (
            "cargo bench -p coseva --features std,parallel --bench parallel -- "
            "--warm-up-time 1 --measurement-time 3 --sample-size 20"
        ),
        "memory": (
            "cargo +nightly -Zscript crates/coseva/scripts/perf_memory.rs "
            "--output target/perf-report/memory.json"
        ),
    }
    for command in read_commands:
        run(command.split())
    run(commands["write"].split())
    run(commands["index"].split())
    run(commands["parallel"].split())

    memory_path = WORKSPACE_ROOT / "target/perf-report/memory.json"
    run(
        [
            "cargo",
            "+nightly",
            "-Zscript",
            str(CRATE_ROOT / "scripts/perf_memory.rs"),
            "--output",
            str(memory_path),
        ]
    )
    memory = json.loads(memory_path.read_text(encoding="utf-8"))
    if not memory["evidence"]["source"].get("tree_clean"):
        raise ValueError("memory artifact was not measured from a clean tree")

    shared = evidence(commands["read"])
    artifact = {
        "schema": SCHEMA,
        "read": {
            "evidence": shared,
            "documents": collect_documents(),
            "counts": collect_matrix(),
        },
        "write": {
            "evidence": {**shared, "command": commands["write"]},
            "counts": collect_callgrind("encode", ("encode",)),
        },
        "parallel": {
            "evidence": {**shared, "command": commands["parallel"]},
            "points": collect_parallel(),
        },
        "index": {
            "evidence": {**shared, "command": commands["index"]},
            "counts": collect_callgrind(
                "index", ("building", "seeking", "bound_seeking")
            ),
        },
        "memory": memory,
        "proc_macro": macro_section(args.macro_artifact.resolve()),
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        json.dumps(artifact, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(f"wrote {args.output}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except subprocess.CalledProcessError as error:
        raise SystemExit(error.returncode) from error
