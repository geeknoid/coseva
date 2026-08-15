#!/usr/bin/env python3

from __future__ import annotations

import copy
import json
import tempfile
import unittest
from pathlib import Path
from typing import Any, Callable

import run as encode_run


Mutation = Callable[[dict[str, Any]], None]


class BaselineValidationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.baseline = json.loads(
            encode_run.DEFAULT_BASELINE.read_text(encoding="utf-8")
        )

    def validate_copy(self, mutation: Mutation | None = None) -> None:
        baseline = copy.deepcopy(self.baseline)
        if mutation is not None:
            mutation(baseline)
        with tempfile.TemporaryDirectory(
            prefix=".baseline-validation-", dir=encode_run.HARNESS_DIR
        ) as directory:
            path = Path(directory) / "baseline.json"
            path.write_text(
                json.dumps(baseline, indent=2) + "\n", encoding="utf-8"
            )
            encode_run.validate_baseline(
                json.loads(path.read_text(encoding="utf-8")), path
            )

    def assert_rejected(self, mutation: Mutation) -> None:
        with self.assertRaises(SystemExit):
            self.validate_copy(mutation)

    def test_valid_schema_2_baseline_passes(self) -> None:
        self.validate_copy()

    def test_tampered_observation_ratio_fails(self) -> None:
        self.assert_rejected(
            lambda baseline: baseline["runs"][0]["paired_evidence"][
                "writer/typical"
            ][0].__setitem__("ratio", 2.0)
        )

    def test_tampered_run_minimum_fails(self) -> None:
        self.assert_rejected(
            lambda baseline: baseline["runs"][0]["ratios"].__setitem__(
                "writer/typical", 2.0
            )
        )

    def test_tampered_floor_observations_fail(self) -> None:
        self.assert_rejected(
            lambda baseline: baseline["ratio_floors"]["writer/typical"][
                "observations"
            ].__setitem__(0, 2.0)
        )

    def test_blocking_promotion_fails(self) -> None:
        def promote(baseline: dict[str, Any]) -> None:
            case = baseline["ratio_floors"]["path/oversized"]
            case.update(
                {
                    "blocking": True,
                    "floor": 1.0,
                    "status": "blocking_at_or_above_parity",
                }
            )

        self.assert_rejected(promote)

    def test_single_run_fails(self) -> None:
        def keep_one_run(baseline: dict[str, Any]) -> None:
            baseline["independent_runs"] = 1
            baseline["runs"] = baseline["runs"][:1]

        self.assert_rejected(keep_one_run)

    def test_incomplete_run_fails(self) -> None:
        self.assert_rejected(
            lambda baseline: baseline["runs"][0]["paired_evidence"].pop(
                "writer/typical"
            )
        )

    def test_nonpositive_elapsed_evidence_fails(self) -> None:
        self.assert_rejected(
            lambda baseline: baseline["runs"][0]["paired_evidence"][
                "writer/typical"
            ][0].__setitem__("coseva_ns", 0)
        )

    def test_nonfinite_elapsed_evidence_fails(self) -> None:
        self.assert_rejected(
            lambda baseline: baseline["runs"][0]["paired_evidence"][
                "writer/typical"
            ][0].__setitem__("csv_ns", float("inf"))
        )


if __name__ == "__main__":
    unittest.main()
