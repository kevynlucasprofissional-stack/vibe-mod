#!/usr/bin/env python3
"""Audit full-file vs chunked Vibe transcripts without conflating repetition with quality.

Without a human/reference transcript this tool reports structural and repetition
*diagnostics only*. It deliberately refuses to call either transcript better.

With ``--reference`` it additionally reports WER, CER and edit breakdowns
(substitutions / insertions / deletions) and can act as a regression gate.
"""

from __future__ import annotations

import argparse
import json
import re
from collections import Counter
from pathlib import Path
from typing import Iterable, Sequence

EPSILON = 1e-9


def normalize(text: str) -> str:
    return " ".join(re.findall(r"\w+", text.lower(), flags=re.UNICODE))


def load_segments(path: Path) -> list[dict]:
    data = json.loads(path.read_text(encoding="utf-8"))
    if isinstance(data, dict):
        data = data.get("segments", [])
    if not isinstance(data, list):
        raise ValueError(f"{path} is not a Vibe transcript JSON")
    return [segment for segment in data if isinstance(segment, dict)]


def segment_texts(segments: Iterable[dict]) -> list[str]:
    texts: list[str] = []
    for segment in segments:
        text = normalize(str(segment.get("text", "")))
        if text:
            texts.append(text)
    return texts


def repetition_metrics(segments: list[dict]) -> dict[str, float | int]:
    texts = segment_texts(segments)
    adjacent = 0
    if len(texts) > 1:
        adjacent = sum(1 for left, right in zip(texts, texts[1:]) if left == right and len(left) >= 6)
    adjacent_ratio = adjacent / max(1, len(texts) - 1)

    counts = Counter(text for text in texts if len(text) >= 6)
    dominant_count = counts.most_common(1)[0][1] if counts else 0
    dominant_ratio = dominant_count / max(1, len(texts)) if dominant_count >= 3 else 0.0

    tokens = " ".join(texts).split()
    trigrams = [tuple(tokens[index : index + 3]) for index in range(max(0, len(tokens) - 2))]
    trigram_repetition = 0.0
    if len(trigrams) >= 10:
        trigram_repetition = 1.0 - len(set(trigrams)) / len(trigrams)

    score = max(adjacent_ratio, dominant_ratio, trigram_repetition)
    return {
        "segments": len(segments),
        "words": len(tokens),
        "adjacent_duplicate_ratio": adjacent_ratio,
        "dominant_segment_ratio": dominant_ratio,
        "trigram_repetition_ratio": trigram_repetition,
        "repetition_score": score,
    }


def _number(value: object) -> float | None:
    try:
        result = float(value)
    except (TypeError, ValueError):
        return None
    if result != result or result in (float("inf"), float("-inf")):
        return None
    return result


def structural_metrics(segments: list[dict]) -> dict[str, float | int | None]:
    valid_intervals: list[tuple[float, float]] = []
    invalid_timestamp_values = 0
    invalid_intervals = 0
    out_of_order = 0
    previous_start: float | None = None

    for segment in segments:
        start = _number(segment.get("start"))
        stop = _number(segment.get("stop", segment.get("end")))
        if start is None or stop is None:
            invalid_timestamp_values += 1
            continue
        if previous_start is not None and start + EPSILON < previous_start:
            out_of_order += 1
        previous_start = start
        if start < -EPSILON or stop + EPSILON < start:
            invalid_intervals += 1
            continue
        valid_intervals.append((start, stop))

    valid_intervals.sort()
    max_gap = 0.0
    previous_stop: float | None = None
    for start, stop in valid_intervals:
        if previous_stop is not None:
            max_gap = max(max_gap, max(0.0, start - previous_stop))
        previous_stop = max(previous_stop or stop, stop)

    first_start = valid_intervals[0][0] if valid_intervals else None
    last_stop = max((stop for _, stop in valid_intervals), default=None)
    span = None if first_start is None or last_stop is None else max(0.0, last_stop - first_start)

    return {
        "valid_timestamped_segments": len(valid_intervals),
        "invalid_timestamp_values": invalid_timestamp_values,
        "invalid_intervals": invalid_intervals,
        "out_of_order_segments": out_of_order,
        "first_start": first_start,
        "last_stop": last_stop,
        "temporal_span": span,
        "max_intersegment_gap": max_gap,
    }


def transcript_text(segments: list[dict]) -> str:
    return " ".join(str(segment.get("text", "")) for segment in segments)


def _advance(state: tuple[int, int, int, int], *, substitution: int = 0, insertion: int = 0, deletion: int = 0) -> tuple[int, int, int, int]:
    distance, substitutions, insertions, deletions = state
    return (
        distance + substitution + insertion + deletion,
        substitutions + substitution,
        insertions + insertion,
        deletions + deletion,
    )


def edit_breakdown(reference: Sequence[str], hypothesis: Sequence[str]) -> dict[str, int | float]:
    """Levenshtein distance with O(len(hypothesis)) memory and edit counts."""
    if not reference:
        insertions = len(hypothesis)
        return {
            "reference_units": 0,
            "hypothesis_units": len(hypothesis),
            "distance": insertions,
            "substitutions": 0,
            "insertions": insertions,
            "deletions": 0,
            "error_rate": 0.0 if not hypothesis else 1.0,
        }

    previous = [(index, 0, index, 0) for index in range(len(hypothesis) + 1)]
    for ref_index, ref_unit in enumerate(reference, start=1):
        current = [(ref_index, 0, 0, ref_index)]
        for hyp_index, hyp_unit in enumerate(hypothesis, start=1):
            if ref_unit == hyp_unit:
                current.append(previous[hyp_index - 1])
                continue
            substitution = _advance(previous[hyp_index - 1], substitution=1)
            insertion = _advance(current[hyp_index - 1], insertion=1)
            deletion = _advance(previous[hyp_index], deletion=1)
            # Prefer lower total distance; ties are deterministic and prefer a
            # substitution over insertion/deletion pairs.
            current.append(min((substitution, insertion, deletion), key=lambda state: (state[0], state[2] + state[3], state[1])))
        previous = current

    distance, substitutions, insertions, deletions = previous[-1]
    return {
        "reference_units": len(reference),
        "hypothesis_units": len(hypothesis),
        "distance": distance,
        "substitutions": substitutions,
        "insertions": insertions,
        "deletions": deletions,
        "error_rate": distance / len(reference),
    }


def accuracy_metrics(reference_text: str, hypothesis_text: str) -> dict[str, dict[str, int | float]]:
    reference_normalized = normalize(reference_text)
    hypothesis_normalized = normalize(hypothesis_text)
    words = edit_breakdown(reference_normalized.split(), hypothesis_normalized.split())
    reference_chars = list(reference_normalized.replace(" ", ""))
    hypothesis_chars = list(hypothesis_normalized.replace(" ", ""))
    characters = edit_breakdown(reference_chars, hypothesis_chars)
    return {"word": words, "character": characters}


def build_result(full_segments: list[dict], chunked_segments: list[dict], reference_text: str | None = None) -> dict[str, object]:
    full_repetition = repetition_metrics(full_segments)
    chunked_repetition = repetition_metrics(chunked_segments)
    full_structure = structural_metrics(full_segments)
    chunked_structure = structural_metrics(chunked_segments)

    result: dict[str, object] = {
        "quality_verdict": "inconclusive_without_reference",
        "full": {"repetition": full_repetition, "structure": full_structure},
        "chunked": {"repetition": chunked_repetition, "structure": chunked_structure},
        "diagnostics": {
            "repetition_score_delta": float(chunked_repetition["repetition_score"]) - float(full_repetition["repetition_score"]),
            "word_count_delta": int(chunked_repetition["words"]) - int(full_repetition["words"]),
            "last_stop_delta": _optional_delta(chunked_structure["last_stop"], full_structure["last_stop"]),
        },
    }

    if reference_text is not None:
        full_accuracy = accuracy_metrics(reference_text, transcript_text(full_segments))
        chunked_accuracy = accuracy_metrics(reference_text, transcript_text(chunked_segments))
        result["full_accuracy"] = full_accuracy
        result["chunked_accuracy"] = chunked_accuracy
        gate, reasons = quality_gate(result, reference_available=True)
        result["quality_gate_passed"] = gate
        result["quality_gate_reasons"] = reasons
        result["quality_verdict"] = "passes_reference_gate" if gate else "fails_reference_gate"
    else:
        gate, reasons = quality_gate(result, reference_available=False)
        result["quality_gate_passed"] = gate
        result["quality_gate_reasons"] = reasons

    return result


def _optional_delta(left: object, right: object) -> float | None:
    if not isinstance(left, (int, float)) or not isinstance(right, (int, float)):
        return None
    return float(left) - float(right)


def quality_gate(result: dict[str, object], *, reference_available: bool) -> tuple[bool, list[str]]:
    reasons: list[str] = []
    full = result["full"]
    chunked = result["chunked"]
    assert isinstance(full, dict) and isinstance(chunked, dict)
    full_rep = full["repetition"]
    chunked_rep = chunked["repetition"]
    chunked_structure = chunked["structure"]
    assert isinstance(full_rep, dict) and isinstance(chunked_rep, dict) and isinstance(chunked_structure, dict)

    if int(full_rep["words"]) > 0 and int(chunked_rep["words"]) == 0:
        reasons.append("chunked transcript is empty while the control contains text")
    for key in ("invalid_timestamp_values", "invalid_intervals", "out_of_order_segments"):
        if int(chunked_structure[key]) > 0:
            reasons.append(f"chunked transcript has {key}={chunked_structure[key]}")

    if not reference_available:
        reasons.append("quality cannot be validated without a reference transcript")
        return False, reasons

    full_accuracy = result["full_accuracy"]
    chunked_accuracy = result["chunked_accuracy"]
    assert isinstance(full_accuracy, dict) and isinstance(chunked_accuracy, dict)
    full_word = full_accuracy["word"]
    chunked_word = chunked_accuracy["word"]
    full_char = full_accuracy["character"]
    chunked_char = chunked_accuracy["character"]
    assert isinstance(full_word, dict) and isinstance(chunked_word, dict)
    assert isinstance(full_char, dict) and isinstance(chunked_char, dict)

    if float(chunked_word["error_rate"]) > float(full_word["error_rate"]) + EPSILON:
        reasons.append("chunked WER is worse than the full-file control")
    if float(chunked_char["error_rate"]) > float(full_char["error_rate"]) + EPSILON:
        reasons.append("chunked CER is worse than the full-file control")
    if int(chunked_word["deletions"]) > int(full_word["deletions"]):
        reasons.append("chunked transcript has more word deletions/omissions than the control")
    if int(chunked_word["insertions"]) > int(full_word["insertions"]):
        reasons.append("chunked transcript has more word insertions than the control")

    return not reasons, reasons


def format_percent(value: float) -> str:
    return f"{value * 100:.2f}%"


def print_report(result: dict[str, object], reference_available: bool) -> None:
    full = result["full"]
    chunked = result["chunked"]
    assert isinstance(full, dict) and isinstance(chunked, dict)
    full_rep = full["repetition"]
    chunked_rep = chunked["repetition"]
    full_structure = full["structure"]
    chunked_structure = chunked["structure"]
    assert isinstance(full_rep, dict) and isinstance(chunked_rep, dict)
    assert isinstance(full_structure, dict) and isinstance(chunked_structure, dict)

    print("Vibe chunking audit")
    print("=" * 78)
    print(f"{'Metric':38} {'Full file':>17} {'Chunked':>17}")
    print("-" * 78)
    rows = [
        ("Segments", "segments", False),
        ("Words", "words", False),
        ("Adjacent duplicates", "adjacent_duplicate_ratio", True),
        ("Dominant repeated segment", "dominant_segment_ratio", True),
        ("Repeated trigrams", "trigram_repetition_ratio", True),
        ("Repetition score", "repetition_score", True),
    ]
    for label, key, percent in rows:
        left = full_rep[key]
        right = chunked_rep[key]
        left_text = format_percent(float(left)) if percent else str(left)
        right_text = format_percent(float(right)) if percent else str(right)
        print(f"{label:38} {left_text:>17} {right_text:>17}")

    print("-" * 78)
    for label, key in [
        ("Invalid timestamp values", "invalid_timestamp_values"),
        ("Invalid intervals", "invalid_intervals"),
        ("Out-of-order segments", "out_of_order_segments"),
        ("Last timestamp", "last_stop"),
        ("Max inter-segment gap", "max_intersegment_gap"),
    ]:
        print(f"{label:38} {str(full_structure[key]):>17} {str(chunked_structure[key]):>17}")

    if reference_available:
        full_accuracy = result["full_accuracy"]
        chunked_accuracy = result["chunked_accuracy"]
        assert isinstance(full_accuracy, dict) and isinstance(chunked_accuracy, dict)
        full_word = full_accuracy["word"]
        chunked_word = chunked_accuracy["word"]
        full_char = full_accuracy["character"]
        chunked_char = chunked_accuracy["character"]
        assert isinstance(full_word, dict) and isinstance(chunked_word, dict)
        assert isinstance(full_char, dict) and isinstance(chunked_char, dict)
        print("-" * 78)
        print(f"{'WER':38} {format_percent(float(full_word['error_rate'])):>17} {format_percent(float(chunked_word['error_rate'])):>17}")
        print(f"{'CER':38} {format_percent(float(full_char['error_rate'])):>17} {format_percent(float(chunked_char['error_rate'])):>17}")
        print(f"{'Word substitutions':38} {str(full_word['substitutions']):>17} {str(chunked_word['substitutions']):>17}")
        print(f"{'Word insertions':38} {str(full_word['insertions']):>17} {str(chunked_word['insertions']):>17}")
        print(f"{'Word deletions / omissions':38} {str(full_word['deletions']):>17} {str(chunked_word['deletions']):>17}")

    print("\nQuality verdict:", result["quality_verdict"])
    reasons = result.get("quality_gate_reasons", [])
    if isinstance(reasons, list):
        for reason in reasons:
            print(f"- {reason}")


def main() -> int:
    parser = argparse.ArgumentParser(description="Audit full-file and protected Vibe transcripts")
    parser.add_argument("full", type=Path, help="JSON transcript produced with protection disabled")
    parser.add_argument("chunked", type=Path, help="JSON transcript produced with protection enabled")
    parser.add_argument("--reference", type=Path, help="UTF-8 human/reference transcript required for a quality verdict")
    parser.add_argument("--json", action="store_true", help="Print machine-readable JSON")
    parser.add_argument(
        "--fail-on-regression",
        action="store_true",
        help="Use the comparison as a quality gate. Requires --reference and fails if WER/CER, omissions or insertions regress.",
    )
    args = parser.parse_args()

    full_segments = load_segments(args.full)
    chunked_segments = load_segments(args.chunked)
    reference_text = args.reference.read_text(encoding="utf-8") if args.reference else None
    result = build_result(full_segments, chunked_segments, reference_text)

    if args.json:
        print(json.dumps(result, indent=2, ensure_ascii=False))
    else:
        print_report(result, reference_text is not None)

    if args.fail_on_regression:
        if reference_text is None:
            print("\nERROR: --fail-on-regression requires --reference; repetition diagnostics alone cannot prove quality.")
            return 2
        return 0 if bool(result["quality_gate_passed"]) else 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
