#!/usr/bin/env python3
from __future__ import annotations

import unittest

from chunking_benchmark import accuracy_metrics, build_result, edit_breakdown, quality_gate


def segment(start: float, stop: float, text: str) -> dict:
    return {"start": start, "stop": stop, "text": text}


class ChunkingBenchmarkTests(unittest.TestCase):
    def test_without_reference_quality_is_always_inconclusive(self) -> None:
        full = [segment(0, 1, "texto normal")]
        chunked = [segment(0, 1, "texto normal")]
        result = build_result(full, chunked)
        self.assertEqual(result["quality_verdict"], "inconclusive_without_reference")
        self.assertFalse(result["quality_gate_passed"])
        self.assertIn("quality cannot be validated without a reference transcript", result["quality_gate_reasons"])

    def test_empty_chunked_transcript_cannot_be_approved_for_lower_repetition(self) -> None:
        full = [segment(i, i + 1, "frase em loop") for i in range(20)]
        result = build_result(full, [])
        self.assertEqual(result["quality_verdict"], "inconclusive_without_reference")
        self.assertFalse(result["quality_gate_passed"])
        self.assertIn("chunked transcript is empty while the control contains text", result["quality_gate_reasons"])
        diagnostics = result["diagnostics"]
        self.assertLess(diagnostics["repetition_score_delta"], 0)

    def test_reference_gate_accepts_equal_correct_transcript(self) -> None:
        reference = "um dois três quatro"
        full = [segment(0, 4, reference)]
        chunked = [segment(0, 4, reference)]
        result = build_result(full, chunked, reference)
        self.assertTrue(result["quality_gate_passed"])
        self.assertEqual(result["quality_verdict"], "passes_reference_gate")

    def test_reference_gate_rejects_new_omission_even_if_output_is_shorter(self) -> None:
        reference = "um dois três quatro cinco"
        full = [segment(0, 5, reference)]
        chunked = [segment(0, 5, "um dois quatro cinco")]
        result = build_result(full, chunked, reference)
        self.assertFalse(result["quality_gate_passed"])
        self.assertIn("chunked transcript has more word deletions/omissions than the control", result["quality_gate_reasons"])

    def test_reference_gate_rejects_new_insertion(self) -> None:
        reference = "um dois três"
        full = [segment(0, 3, reference)]
        chunked = [segment(0, 3, "um dois extra três")]
        result = build_result(full, chunked, reference)
        self.assertFalse(result["quality_gate_passed"])
        self.assertIn("chunked transcript has more word insertions than the control", result["quality_gate_reasons"])

    def test_invalid_or_out_of_order_timestamps_fail_gate(self) -> None:
        reference = "um dois"
        full = [segment(0, 1, "um"), segment(1, 2, "dois")]
        chunked = [segment(1, 2, "dois"), segment(0, 1, "um")]
        result = build_result(full, chunked, reference)
        self.assertFalse(result["quality_gate_passed"])
        self.assertIn("chunked transcript has out_of_order_segments=1", result["quality_gate_reasons"])

    def test_edit_breakdown_reports_word_error_components(self) -> None:
        metrics = edit_breakdown("a b c".split(), "a x c extra".split())
        self.assertEqual(metrics["distance"], 2)
        self.assertEqual(metrics["substitutions"], 1)
        self.assertEqual(metrics["insertions"], 1)
        self.assertEqual(metrics["deletions"], 0)

    def test_accuracy_reports_zero_wer_and_cer_for_exact_match(self) -> None:
        metrics = accuracy_metrics("Olá, mundo!", "olá mundo")
        self.assertEqual(metrics["word"]["error_rate"], 0.0)
        self.assertEqual(metrics["character"]["error_rate"], 0.0)

    def test_quality_gate_function_refuses_no_reference_even_for_identical_outputs(self) -> None:
        result = build_result([segment(0, 1, "a")], [segment(0, 1, "a")])
        passed, reasons = quality_gate(result, reference_available=False)
        self.assertFalse(passed)
        self.assertTrue(reasons)


if __name__ == "__main__":
    unittest.main()
