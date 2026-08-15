#!/usr/bin/env python3
"""Run the deterministic pull-request benchmark sentinels."""

from __future__ import annotations

import json
import subprocess
import sys
from collections import defaultdict
from dataclasses import dataclass
from fractions import Fraction
from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]
BASELINES = Path(__file__).with_name("perf-baselines.tsv")
# Which runtime-selected kernel arm the sentinels are expected to have measured,
# and where `benches/dispatch.rs` reports the arm it actually reached.
DISPATCH_ARM = Path(__file__).with_name("perf-dispatch-arm.txt")
MEASURED_ARM = ROOT / "target/perf-report/dispatch-arm.txt"
LIMIT = Fraction(102, 100)


@dataclass(frozen=True)
class Sample:
    module: str
    ident: str | None
    coefficient: int = 1


@dataclass(frozen=True)
class Case:
    bench: str
    features: str
    filter: str
    key: str
    samples: tuple[Sample, ...]
    divisor: int = 1
    blocking: bool = True


def direct(
    bench: str,
    features: str,
    filter: str,
    module: str,
    ident: str | None = None,
    *,
    blocking: bool = True,
) -> Case:
    # A benchmark declared without a `#[bench::...]` case carries no id, so its
    # key is the module path alone.
    return Case(
        bench,
        features,
        filter,
        f"{module}::{ident}" if ident is not None else module,
        (Sample(module, ident),),
        blocking=blocking,
    )


def marginal(
    bench: str,
    features: str,
    filter: str,
    module: str,
) -> Case:
    return Case(
        bench,
        features,
        filter,
        f"{module}::marginal_100_1000",
        (
            Sample(module, "rows_100", -1),
            Sample(module, "rows_1000"),
        ),
        divisor=900,
    )


CASES = (
    direct(
        "matrix",
        "std,derive,serde",
        "*record_slice*metrics",
        "matrix::matrix::record_slice",
        "metrics",
    ),
    direct(
        "quoted",
        "std",
        "*slice*interior_1000",
        "quoted::quoted::slice",
        "interior_1000",
    ),
    direct(
        "window",
        "std,multibyte",
        "*io_long_multibyte*bytes_32",
        "window::window::io_long_multibyte",
        "bytes_32",
    ),
    direct(
        "mapping",
        "std,serde",
        "*mapping::mapping::*w200",
        "mapping::mapping::map_all",
        "w200",
    ),
    direct(
        "mapping",
        "std,serde",
        "*mapping::mapping::*w200",
        "mapping::mapping::map_one",
        "w200",
    ),
    direct(
        "mapping",
        "std,serde",
        "*mapping::mapping::*w200",
        "mapping::mapping::map_two",
        "w200",
    ),
    # The narrow and wide Serde cache paths. Marginal rows cancel the first
    # record's learning cost and guard the steady-state ignored-column skip.
    *(
        marginal(
            "mapping",
            "std,serde",
            "*wide_select::*rows_100*",
            f"mapping::wide_select::{name}",
        )
        for name in ("select_two_narrow", "select_two_wide")
    ),
    # Applying one resolved projection per record through every public record
    # representation. These guard indexed access independently of resolution.
    *(
        marginal(
            "mapping",
            "std,serde",
            "*projection::*rows_100*",
            f"mapping::projection::{name}",
        )
        for name in (
            "project_byte_record",
            "project_text_record",
            "project_lending_record",
        )
    ),
    direct(
        "index",
        "std,index,derive",
        "*seeking*seek*at_last",
        "index::seeking::seek",
        "at_last",
    ),
    direct(
        "literal_search",
        "std",
        "*absent*",
        "literal_search::literal_search::absent_needle",
        "rare_leading",
    ),
    direct(
        "literal_search",
        "std",
        "*absent*",
        "literal_search::literal_search::absent_needle",
        "common_leading",
    ),
    *(
        direct(
            "literal_search",
            "std",
            "*positioned_search*",
            "literal_search::literal_search::positioned_search",
            case,
        )
        for case in ("present_early", "present_late", "near_hit_absent")
    ),
    direct(
        "startup",
        "std",
        "*construct_200*",
        "startup::startup::construct_200",
        "w",
    ),
    # coseva owns the gate; csv is a recorded, report-only dependency.
    *(
        direct(
            "matrix_byte_record_io",
            "std,derive,serde",
            "*byte_record*",
            "matrix_byte_record_io::matrix_byte_record_io::byte_record_io",
            document,
        )
        for document in ("metrics", "wide", "quoted", "prose", "spreadsheet")
    ),
    *(
        direct(
            "matrix_byte_record_io",
            "std,derive,serde",
            "*byte_record*",
            "matrix_byte_record_io::matrix_byte_record_io::byte_record_csv",
            document,
            blocking=False,
        )
        for document in ("metrics", "wide", "quoted", "prose", "spreadsheet")
    ),
    # Borrowed slices across all emitters, plus native and Serde typed rows.
    *(
        marginal(
            "encode",
            "std,derive,serde",
            "*encode::encode::*rows_100*",
            f"encode::encode::{name}",
        )
        for name in ("vec", "push", "io", "vec_encode", "vec_serialize")
    ),
    # The field-at-a-time builder at both widths. It stages each record in a
    # `ByteRecord` held on the emitter; without that reuse these rows carry two
    # allocations and their growth per record, which is most of their cost.
    *(
        marginal(
            "encode",
            "std,derive,serde",
            f"*staging::{name}*rows_100*",
            f"encode::staging::{name}",
        )
        for name in ("narrow_builder", "wide_builder")
    ),
    # The non-numeric quoting mode, whose per-field numeric recognition is a
    # byte scan rather than a parse. Only this mode consults it, so nothing else
    # would notice the scan regressing back to a full parse.
    direct(
        "encode",
        "std,derive,serde",
        "*quoting_non_numeric*",
        "encode::activated::quoting_non_numeric",
        "rows_1000",
    ),
    *(
        direct(
            "encode",
            "std,derive,serde",
            "*sink_backed*sink_drain*",
            "encode::sink_backed::sink_drain",
            case,
        )
        for case in (
            "io_full_small",
            "io_partial_small",
            "encode_full_threshold",
            "encode_partial_threshold",
            "io_full_oversized",
            "encode_partial_oversized",
        )
    ),
    # All index construction modes, including fast and parser-fallback generation.
    *(
        marginal(
            "index",
            "std,index,derive",
            "*building*rows_100*",
            f"index::building::{name}",
        )
        for name in ("build", "create", "generate")
    ),
    marginal(
        "index",
        "std,index,derive",
        "*generate_raw*rows_100*",
        "index::generate_shapes::generate_raw",
    ),
    # Together these span selectivity, position, predicate and front end.
    direct(
        "filter",
        "std",
        "*filtered*all",
        "filter::equals::equals_filtered",
        "all",
    ),
    direct(
        "filter",
        "std",
        "*filtered*thousandth",
        "filter::equals::push_equals_filtered",
        "thousandth",
    ),
    direct(
        "filter",
        "std",
        "*filtered*thousandth",
        "filter::contains::late_contains_filtered",
        "thousandth",
    ),
    direct(
        "filter",
        "std",
        "*filtered*all",
        "filter::contains::push_late_contains_filtered",
        "all",
    ),
    # The io front end, whose pushdown is a separate implementation from the
    # slice one, and the push front end at a chunk size where the per-chunk copy
    # no longer hides what the filter does.
    direct(
        "filter",
        "std",
        "*filtered*thousandth",
        "filter::equals::io_equals_filtered",
        "thousandth",
    ),
    direct(
        "filter",
        "std",
        "*filtered*thousandth",
        "filter::equals::push_equals_filtered_large",
        "thousandth",
    ),
    # Long records, where the skip scan rather than the parse it avoids is the
    # cost. The `late` row is the only one that makes the backward terminator
    # search widen past its initial window.
    *(
        direct(
            "filter",
            "std",
            "*long_*sparse*",
            f"filter::equals::{name}",
            "sparse",
        )
        for name in ("long_equals_filtered", "long_late_equals_filtered")
    ),
    # Six escaped quoted fields per record, which is what drives the vectorized
    # parser's masked multi-quote branch.
    direct(
        "quoted",
        "std",
        "*slice::dense_1000",
        "quoted::quoted::slice",
        "dense_1000",
    ),
    # One representative row per otherwise ungated Callgrind bench. Each bench
    # costs one filtered group run, so rows drawn from the same run are close to
    # free and the selection is per bench rather than per case.
    #
    # The typed surface leads: the native borrowed rows exercise mapped
    # `ResolvedSpans` through all three front ends, while
    # `slice_serde_borrowed` covers the Serde adapter beside them in the same
    # binary. Together the group spans transport and derive costs.
    *(
        marginal(
            "decode",
            "std,derive,serde,compact_str",
            "*decode::borrowed::*rows_100*",
            f"decode::borrowed::{name}",
        )
        for name in (
            "slice_borrowed",
            "io_borrowed",
            "push_borrowed",
            "slice_serde_borrowed",
        )
    ),
    marginal(
        "decode_wide",
        "std,derive",
        "*decode_wide::borrowed::slice_borrowed*rows_100*",
        "decode_wide::borrowed::slice_borrowed",
    ),
    marginal(
        "deserialize",
        "std,serde",
        "*deserialize::borrowed::slice_borrowed*rows_100*",
        "deserialize::borrowed::slice_borrowed",
    ),
    # The untyped record surface: borrowed spans, owned bytes and owned text.
    marginal(
        "read_record",
        "std",
        "*read_record::read_record::slice*rows_100*",
        "read_record::read_record::slice",
    ),
    marginal(
        "byte_record",
        "std",
        "*byte_record::byte_record::slice*rows_100*",
        "byte_record::byte_record::slice",
    ),
    marginal(
        "text_record",
        "std",
        "*text_record::text_record::slice*rows_100*",
        "text_record::text_record::slice",
    ),
    # Rewriting a record field by field at the widest shape. Guards the
    # equal-length short circuit in `set_field`, without which this row is
    # quadratic in field count rather than linear.
    direct(
        "set_field",
        "std",
        "*set_field::set_field::equal*width_128*",
        "set_field::set_field::equal",
        "width_128",
    ),
    # Two documents from the corpus matrix rather than all five: the plain one
    # and the quoted one, which is the only shape with a distinct kernel.
    *(
        direct(
            "matrix_byte_record",
            "std,derive,serde",
            "*byte_record_slice*",
            "matrix_byte_record::matrix_byte_record::byte_record_slice",
            document,
        )
        for document in ("metrics", "quoted")
    ),
    # The widest row in the sweep, where per-field cost dominates per-record.
    marginal(
        "width_sweep",
        "std,derive,serde",
        "*width::slice_200*",
        "width_sweep::width::slice_200",
    ),
    # `materialization` already carries its own hard Ir limits on the scan,
    # borrowed and typed cases; only the owned ones are unguarded.
    direct(
        "materialization",
        "std,derive,benchmarking",
        "*owned_200*",
        "materialization::materialization::owned_200",
    ),
    direct(
        "decode_into",
        "std,derive",
        "*wide_decode_into*",
        "decode_into::decode_into::wide_decode_into",
    ),
    # The bulk owned-decode iterator, at both widths, in header order. The
    # reordered pair costs about five instructions per field more and is left
    # out rather than gated twice for the same code.
    *(
        marginal(
            "bulk_decode",
            "std,derive",
            "*bulk::*_ordered*rows_100*",
            f"bulk_decode::bulk::{name}",
        )
        for name in ("narrow_ordered", "wide_ordered")
    ),
    # The general dialect engine, which is the path every non-CSV dialect takes,
    # and the reference rows the dialect cost multiples in the public API docs
    # are stated against. Cross-file comparisons are meaningless here (see
    # `benches/fixture.rs`), so every one of these is measured in this binary.
    *(
        marginal(
            "dialects",
            "std,multibyte",
            "*dialects::dialects::*rows_100*",
            f"dialects::dialects::{name}",
        )
        for name in (
            "specialized",
            "specialized_static",
            "general_crlf",
            "general_mysql_escape",
        )
    ),
    # `Whitespace::unquoted_only` on a record that actually contains a quoted
    # field, which is the only shape that leaves the vectorized path, against
    # the same bytes read under a dialect that asks for no trimming.
    *(
        direct(
            "dialects",
            "std,multibyte",
            "*triggered::trim_quoted*",
            f"dialects::triggered::{name}",
            "rows_1000",
        )
        for name in ("trim_quoted_fallback", "trim_quoted_control")
    ),
    # The quoting predicate either side of `SIMD_QUOTING_SCAN_BYTES`. The two
    # `dispatch` rows are the arms emission actually takes, so they are gated
    # outright; the `blocks` and `words` rows exist to feed the crossover
    # ratios, which is where the threshold's justification is enforced, so they
    # are measured without a band of their own.
    *(
        direct(
            "needs_quotes",
            "std,benchmarking",
            "*quoting::newline_*",
            "needs_quotes::quoting::newline_dispatch",
            width,
        )
        for width in ("w24", "w32")
    ),
    *(
        direct(
            "needs_quotes",
            "std,benchmarking",
            "*quoting::newline_*",
            f"needs_quotes::quoting::newline_{arm}",
            width,
            blocking=False,
        )
        for arm in ("blocks", "words")
        for width in ("w16", "w32")
    ),
    # The same structural count through the fallback and through whichever
    # kernel this environment dispatches to. Neither absolute number is gated —
    # the point is the ratio between them, which says whether the vector arm is
    # delivering anything in the environment being measured, and which cannot be
    # right for the wrong reason the way a detection flag can.
    #
    # Running this group also writes `MEASURED_ARM`, because the `arm` case in
    # the same binary reports its detection from inside the profiled process.
    # That file is what `check_dispatch_arm` reads, so these two cases are what
    # keeps the arm check running at all.
    *(
        direct(
            "dispatch",
            "std,benchmarking",
            "*dispatch::*",
            f"dispatch::dispatch::{arm}",
            blocking=False,
        )
        for arm in ("scalar", "selected")
    ),
)


@dataclass(frozen=True)
class Ratio:
    """A performance property asserted in prose, pinned to two measured rows.

    An aggregate row cannot protect a claim about the relationship between two
    paths: a change that moved both would leave every row inside its 2% band
    and still make the sentence false.

    Counterfactual claims — "omitting this `#[inline]` costs 3%" — are outside
    what any gate can express, because the gate only ever measures the code as
    written. Those are recorded one-off observations, and say so.
    """

    key: str
    numerator: str
    denominator: str
    low: Fraction
    high: Fraction
    claim: str


RATIOS = (
    Ratio(
        "common_over_rare_literal_search",
        "literal_search::literal_search::absent_needle::common_leading",
        "literal_search::literal_search::absent_needle::rare_leading",
        Fraction(98, 100),
        Fraction(102, 100),
        "benches/literal_search.rs — the anchor cases remain cost-equivalent",
    ),
    Ratio(
        "crlf_over_specialized",
        "dialects::dialects::general_crlf::marginal_100_1000",
        "dialects::dialects::specialized::marginal_100_1000",
        Fraction(135, 100),
        Fraction(150, 100),
        "src/config/record_ending.rs — CrLf costs about 1.4x Newline",
    ),
    Ratio(
        "mysql_escape_over_specialized",
        "dialects::dialects::general_mysql_escape::marginal_100_1000",
        "dialects::dialects::specialized::marginal_100_1000",
        Fraction(128, 100),
        Fraction(143, 100),
        "src/config/escape.rs — escape-free MySQL input costs about 1.35x",
    ),
    Ratio(
        "trim_quoted_fallback_over_control",
        "dialects::triggered::trim_quoted_fallback::rows_1000",
        "dialects::triggered::trim_quoted_control::rows_1000",
        Fraction(200, 100),
        Fraction(225, 100),
        "src/config/whitespace.rs — the general-parser fallback costs about 2.1x",
    ),
    Ratio(
        "selected_over_scalar_scan",
        "dispatch::dispatch::selected",
        "dispatch::dispatch::scalar",
        Fraction(20, 100),
        Fraction(28, 100),
        "docs/DESIGN.md — the dispatched structural scan beats the fallback about 4x",
    ),
    Ratio(
        "quoting_blocks_over_words_w32",
        "needs_quotes::quoting::newline_blocks::w32",
        "needs_quotes::quoting::newline_words::w32",
        Fraction(33, 100),
        Fraction(40, 100),
        "src/emit.rs — at the threshold width the block scan is the cheaper arm",
    ),
    Ratio(
        "quoting_blocks_over_words_w16",
        "needs_quotes::quoting::newline_blocks::w16",
        "needs_quotes::quoting::newline_words::w16",
        Fraction(130, 100),
        Fraction(152, 100),
        "src/emit.rs — under one block the block scan is the dearer arm",
    ),
    Ratio(
        "dynamic_over_static_format",
        "dialects::dialects::specialized::marginal_100_1000",
        "dialects::dialects::specialized_static::marginal_100_1000",
        Fraction(133, 100),
        Fraction(149, 100),
        "benches/dialects.rs — carrying the format in a value costs about 41%",
    ),
)


def check_dispatch_arm() -> str | None:
    """Compare the arm the profiled run reached against the recorded one.

    Every vector kernel here is chosen by runtime CPU detection, and the
    sentinels run under Valgrind, which emulates the guest CPU and answers
    `CPUID` itself. So the arm is a property of the Valgrind version and the CI
    image, not of the source, and it can change under a green build: if the
    emulated CPUID stops reporting AVX2 then every baseline in this file
    silently re-pins the scalar fallback and the vector kernels are guarded by
    nothing.

    Returns a failure message, or `None` when the arm matches.
    """
    if not MEASURED_ARM.exists():
        return (
            f"{MEASURED_ARM} was not written; the dispatch benchmark did not run"
        )
    measured = MEASURED_ARM.read_text(encoding="utf-8").strip()
    expected = next(
        line.strip()
        for line in DISPATCH_ARM.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.startswith("#")
    )
    if measured != expected:
        return (
            f"dispatch arm changed from `{expected}` to `{measured}`;"
            " every baseline in this file was refreshed under the recorded arm,"
            " so they measure different code now"
        )
    return None


def metric(summary: dict) -> int:
    metrics = summary["profiles"][0]["summaries"]["total"]["summary"]["Callgrind"]["Ir"][
        "metrics"
    ]
    values = next(iter(metrics.values()))
    value = values[0] if isinstance(values, list) else values
    return int(value["Int"])


def run_group(bench: str, features: str, filter: str) -> dict[tuple[str, str], int]:
    command = [
        "cargo",
        "bench",
        "-p",
        "coseva",
        "--bench",
        bench,
        "--features",
        features,
        "--",
        filter,
        "--output-format=json",
    ]
    completed = subprocess.run(
        command,
        cwd=ROOT,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    )
    measured = {}
    for line in completed.stdout.splitlines():
        summary = json.loads(line)
        key = (summary["module_path"], summary["id"])
        if key in measured:
            raise RuntimeError(f"{key[0]}::{key[1]}: duplicate JSON summary")
        measured[key] = metric(summary)
    return measured


def measure_all() -> dict[str, Fraction]:
    known = {case.key for case in CASES}
    for ratio in RATIOS:
        for key in (ratio.numerator, ratio.denominator):
            if key not in known:
                raise RuntimeError(f"{ratio.key}: no measured case {key}")

    groups: dict[tuple[str, str, str], list[Case]] = defaultdict(list)
    for case in CASES:
        groups[(case.bench, case.features, case.filter)].append(case)

    measured = {}
    for command, cases in groups.items():
        samples = run_group(*command)
        for case in cases:
            total = 0
            for sample in case.samples:
                key = (sample.module, sample.ident)
                if key not in samples:
                    raise RuntimeError(
                        f"{case.key}: missing JSON summary {sample.module}::{sample.ident}"
                    )
                total += sample.coefficient * samples[key]
            value = Fraction(total, case.divisor)
            if value <= 0:
                raise RuntimeError(f"{case.key}: non-positive measurement {value}")
            measured[case.key] = value
    return measured


def read_baselines() -> dict[str, Fraction]:
    values = {}
    for line in BASELINES.read_text().splitlines():
        if not line or line.startswith("#"):
            continue
        key, value = line.split("\t")
        values[key] = Fraction(value)
    return values


def stored(value: Fraction) -> str:
    if value.denominator == 1:
        return str(value.numerator)
    return f"{value.numerator}/{value.denominator}"


def displayed(value: Fraction) -> str:
    if value.denominator == 1:
        return f"{value.numerator:,}"
    return f"{float(value):,.3f}"


def main() -> int:
    refresh = sys.argv[1:] == ["--refresh"]
    if sys.argv[1:] not in ([], ["--refresh"]):
        print("usage: perf_gate.py [--refresh]", file=sys.stderr)
        return 2

    measured = measure_all()
    if refresh:
        print("# Gungraun Callgrind Ir baselines; refresh only on the named CI toolchain.")
        print("# Marginal values are exact (rows_1000 - rows_100) / 900 fractions.")
        print("# Run: crates/coseva/scripts/perf_gate.py --refresh")
        for case in CASES:
            print(f"{case.key}\t{stored(measured[case.key])}")
        for ratio in RATIOS:
            observed = measured[ratio.numerator] / measured[ratio.denominator]
            print(f"# ratio {ratio.key}: {float(observed):.3f}x", file=sys.stderr)
        if MEASURED_ARM.exists():
            arm = MEASURED_ARM.read_text(encoding="utf-8").strip()
            print(
                f"# dispatch arm: {arm} — write this to {DISPATCH_ARM.name}",
                file=sys.stderr,
            )
        return 0

    baselines = read_baselines()
    failed = False
    for case in CASES:
        old = baselines.get(case.key)
        if old is None:
            raise RuntimeError(f"{case.key}: no checked baseline")
        new = measured[case.key]
        change = float((new - old) / old * 100)
        if case.blocking:
            status = "ok"
            if new > old * LIMIT:
                status = "REGRESSION"
                failed = True
        else:
            status = "dependency"
        unit = "Ir/record" if case.divisor != 1 else "Ir"
        print(f"{status:10} {case.key}: {displayed(new)} {unit} ({change:+.2f}%)")

    for ratio in RATIOS:
        observed = measured[ratio.numerator] / measured[ratio.denominator]
        status = "ok"
        if not ratio.low <= observed <= ratio.high:
            status = "CLAIM BROKEN"
            failed = True
        print(
            f"{status:10} {ratio.key}: {float(observed):.3f}x"
            f" (expected {float(ratio.low):.2f}-{float(ratio.high):.2f}x)"
            f" — {ratio.claim}"
        )
    arm_failure = check_dispatch_arm()
    if arm_failure is None:
        arm = MEASURED_ARM.read_text(encoding="utf-8").strip()
        status = "ok"
        detail = f"dispatch arm: {arm}"
    else:
        status = "ARM MOVED"
        detail = arm_failure
        failed = True
    print(f"{status:10} {detail}")

    return 1 if failed else 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"perf_gate: {error}", file=sys.stderr)
        raise SystemExit(1)
