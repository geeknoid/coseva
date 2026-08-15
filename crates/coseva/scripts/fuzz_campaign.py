#!/usr/bin/env python3
"""Run one coverage-guided libFuzzer campaign target described by campaign.toml.

`crates/coseva/tests/__fuzz__/campaign.toml` remains the single source of truth
for the target list, its test binary, its features and the shared budget; this
script only turns one `[[target]]` entry into a command and runs it.

Why this exists rather than `cargo bolero test`
-----------------------------------------------
`cargo-bolero` 0.13.4 unconditionally adds `-Cpasses=sancov-module` to
RUSTFLAGS and builds with a bare test-name filter rather than `--test <binary>`,
which has two consequences:

* Combined with `-Zsanitizer=address`, that legacy-pass-manager route to
  SanitizerCoverage makes rustc emit ASan destructor references to
  `__sancov_gen_*` globals that nothing defines, so the campaign fails at link
  time. This reproduces on stable 1.97.1, on nightly-2026-05-29 and on
  nightly-2026-06-22, so it is not a toolchain-pin question; and because
  cargo-bolero only ever *appends* to a pre-existing RUSTFLAGS, its injection
  cannot be removed from the outside.
* Because every test binary in the package is built, `--sanitizer NONE` fails
  too: the non-fuzz binaries get the coverage instrumentation but never link the
  libFuzzer runtime that defines `__sanitizer_cov_*`.

Dropping ASan and scoping the build to the campaign's own `--test` target avoids
both. That is what this script does, so the campaign is coverage-guided and
actually runs. The cost is that ASan's memory-error detection is not part of the
campaign; the Miri job in ci.yml covers the unsafe kernels on every pull request
instead.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
CAMPAIGN = REPO_ROOT / "crates/coseva/tests/__fuzz__/campaign.toml"

# The SanitizerCoverage instrumentation libFuzzer needs in order to be
# coverage-guided at all. Without these the binary links but libFuzzer refuses
# to start, reporting that it found no interesting inputs.
SANCOV_RUSTFLAGS = [
    "--cfg",
    "fuzzing",
    "--cfg",
    "fuzzing_libfuzzer",
    "-Cpasses=sancov-module",
    "-Cllvm-args=-sanitizer-coverage-level=4",
    "-Cllvm-args=-sanitizer-coverage-inline-8bit-counters",
    "-Cllvm-args=-sanitizer-coverage-pc-table",
    "-Cllvm-args=-sanitizer-coverage-trace-compares",
    "-Cllvm-args=-sanitizer-coverage-stack-depth",
]


def load_campaign() -> tuple[dict, list[dict]]:
    with CAMPAIGN.open("rb") as f:
        doc = tomllib.load(f)
    info = doc.get("campaign", {})
    targets = doc.get("target", [])
    if not targets:
        sys.exit(f"{CAMPAIGN}: no [[target]] entries found")
    for key in ("package", "default_time", "default_max_input_length", "corpus_root"):
        if key not in info:
            sys.exit(f"{CAMPAIGN}: [campaign] is missing key {key!r}")
    return info, targets


def host_triple() -> str:
    out = subprocess.run(
        ["rustc", "-vV"], check=True, capture_output=True, text=True
    ).stdout
    for line in out.splitlines():
        if line.startswith("host: "):
            return line[len("host: ") :].strip()
    sys.exit("could not determine the host target triple from `rustc -vV`")


def build(target: dict, package: str, build_dir: Path) -> Path:
    """Build the campaign's test binary with coverage instrumentation.

    An explicit `--target` is required: without it Cargo would apply the
    instrumentation RUSTFLAGS to build scripts and proc macros too, which then
    fail to link for want of the libFuzzer runtime.
    """
    env = dict(os.environ)
    env["BOLERO_FUZZER"] = "libfuzzer"
    env["RUSTFLAGS"] = " ".join(SANCOV_RUSTFLAGS + [env.get("RUSTFLAGS", "")]).strip()
    # bolero's libfuzzer engine is nightly-only machinery; this lets the
    # ordinary stable toolchain build it, matching what campaign.toml documents.
    env["RUSTC_BOOTSTRAP"] = "1"

    cmd = [
        "cargo",
        "test",
        "--target",
        host_triple(),
        "--profile",
        "fuzz",
        "--package",
        package,
        "--test",
        target["test"],
        "--target-dir",
        str(build_dir),
        "--message-format=json-render-diagnostics",
        "--no-run",
    ]
    if target["features"]:
        cmd += ["--features", ",".join(target["features"])]

    proc = subprocess.run(cmd, cwd=REPO_ROOT, env=env, capture_output=True, text=True)
    sys.stderr.write(proc.stderr)
    if proc.returncode != 0:
        sys.exit(f"build failed for campaign target {target['name']!r}")

    executable = None
    for line in proc.stdout.splitlines():
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        if (
            msg.get("reason") == "compiler-artifact"
            and msg.get("target", {}).get("name") == target["test"]
            and msg.get("executable")
        ):
            executable = msg["executable"]
    if executable is None:
        sys.exit(f"cargo produced no executable for test binary {target['test']!r}")
    return Path(executable)


def run(target: dict, info: dict, binary: Path, seconds: int, work: Path) -> int:
    name = target["name"]
    # `name` is the corpus directory; the item path libFuzzer's harness filter
    # needs spells a nested module with `::` instead of `__`.
    item_path = name.replace("__", "::")

    seed_corpus = REPO_ROOT / info["corpus_root"] / name / "corpus"
    if not seed_corpus.is_dir():
        sys.exit(f"{seed_corpus}: campaign target {name!r} has no committed corpus")
    seeds = sorted(p for p in seed_corpus.iterdir() if p.is_file())

    grown_corpus = work / "corpus"
    crashes = REPO_ROOT / info["corpus_root"] / name / "crashes"
    grown_corpus.mkdir(parents=True, exist_ok=True)
    crashes.mkdir(parents=True, exist_ok=True)

    print(
        f"campaign {name}: {len(seeds)} seed input(s), {seconds}s budget, "
        f"crashes -> {crashes.relative_to(REPO_ROOT)}",
        flush=True,
    )

    env = dict(os.environ)
    env["BOLERO_TEST_NAME"] = item_path
    env["BOLERO_LIBTEST_HARNESS"] = "1"
    env["BOLERO_LIBFUZZER_ARGS"] = " ".join(
        [
            str(grown_corpus),
            str(seed_corpus),
            f"-artifact_prefix={crashes}/",
            f"-max_total_time={seconds}",
            f"-max_len={info['default_max_input_length']}",
            "-timeout=10",
            "-print_final_stats=1",
        ]
    )

    proc = subprocess.run(
        [str(binary), item_path, "--exact", "--nocapture", "--test-threads", "1"],
        cwd=REPO_ROOT,
        env=env,
    )
    return proc.returncode


def main() -> int:
    info, targets = load_campaign()
    names = [t["name"] for t in targets]

    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("target", choices=names, help="campaign target to run")
    parser.add_argument(
        "--seconds",
        type=int,
        default=int(str(info["default_time"]).rstrip("s")),
        help="wall-clock budget (default: campaign.toml's [campaign].default_time)",
    )
    parser.add_argument(
        "--keep-build",
        action="store_true",
        help="retain the instrumented build directory for inspection",
    )
    args = parser.parse_args()

    target = next(t for t in targets if t["name"] == args.target)
    for key in ("test", "features"):
        if key not in target:
            sys.exit(f"{CAMPAIGN}: target {args.target!r} is missing key {key!r}")

    # The instrumentation RUSTFLAGS differ from every other build in the
    # workspace, so a shared target directory would force a full rebuild of the
    # ordinary one on the next `cargo test`.
    work = REPO_ROOT / "target/fuzz" / args.target
    build_dir = work / "build"
    work.mkdir(parents=True, exist_ok=True)

    binary = build(target, info["package"], build_dir)
    try:
        return run(target, info, binary, args.seconds, work)
    finally:
        if not args.keep_build:
            shutil.rmtree(build_dir, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
