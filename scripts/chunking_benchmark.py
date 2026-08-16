#!/usr/bin/env python3
"""Compare full-file and chunked Vibe transcripts for repetition and accuracy.

Expected transcript inputs are Vibe JSON exports (a list of segment objects or an
object with a ``segments`` list). An optional plain-text reference enables WER.

Example:
    python scripts/chunking_benchmark.py full.json chunked.json --reference reference.txt
"""

from __future__ import annotations

import argparse
import json
import re
from collections import Counter
from pathlib import Path
from typing import Iterable


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
    return [normalize(str(segment.get("text", ""))) for segment in segments if normalize(str(segment.get("text", "")))]


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

    duration = 0.0
    for segment in segments:
        try:
            duration = max(duration, float(segment.get("stop", segment.get("end", 0.0))))
        except (TypeError, ValueError):
            pass

    score = max(adjacent_ratio, dominant_ratio, trigram_repetition)
    return {
        "segments": len(segments),
        "words": len(tokens),
        "duration": duration,
        "adjacent_duplicate_ratio": adjacent_ratio,
        "dominant_segment_ratio": dominant_ratio,
        "trigram_repetition_ratio": trigram_repetition,
        "repetition_score": score,
    }


def word_error_rate(reference: str, hypothesis: str) -> float:
    ref = normalize(reference).split()
    hyp = normalize(hypothesis).split()
    if not ref:
        return 0.0 if not hyp else 1.0

    previous = list(range(len(hyp) + 1))
    for ref_index, ref_word in enumerate(ref, start=1):
        current = [ref_index]
        for hyp_index, hyp_word in enumerate(hyp, start=1):
            substitution = previous[hyp_index - 1] + (ref_word != hyp_word)
            insertion = current[hyp_index - 1] + 1
            deletion = previous[hyp_index] + 1
            current.append(min(substitution, insertion, deletion))
        previous = current
    return previous[-1] / len(ref)


def transcript_text(segments: list[dict]) -> str:
    return " ".join(str(segment.get("text", "")) for segment in segments)


def format_percent(value: float) -> str:
    return f"{value * 100:.2f}%"


def main() -> int:
    parser = argparse.ArgumentParser(description="Compare full-file and 30-second chunked Vibe transcripts")
    parser.add_argument("full", type=Path, help="JSON transcript produced with chunk protection disabled")
    parser.add_argument("chunked", type=Path, help="JSON transcript produced with 30-second chunk protection enabled")
    parser.add_argument("--reference", type=Path, help="Optional UTF-8 reference transcript for WER")
    parser.add_argument("--json", action="store_true", help="Print machine-readable JSON")
    parser.add_argument(
        "--fail-on-regression",
        action="store_true",
        help="Exit non-zero when chunked repetition is worse, or WER is worse when a reference is supplied",
    )
    args = parser.parse_args()

    full_segments = load_segments(args.full)
    chunked_segments = load_segments(args.chunked)
    full_metrics = repetition_metrics(full_segments)
    chunked_metrics = repetition_metrics(chunked_segments)

    result: dict[str, object] = {
        "full": full_metrics,
        "chunked": chunked_metrics,
        "repetition_score_delta": float(chunked_metrics["repetition_score"]) - float(full_metrics["repetition_score"]),
    }

    if args.reference:
        reference = args.reference.read_text(encoding="utf-8")
        full_wer = word_error_rate(reference, transcript_text(full_segments))
        chunked_wer = word_error_rate(reference, transcript_text(chunked_segments))
        result["full_wer"] = full_wer
        result["chunked_wer"] = chunked_wer
        result["wer_delta"] = chunked_wer - full_wer

    if args.json:
        print(json.dumps(result, indent=2, ensure_ascii=False))
    else:
        print("Vibe chunking benchmark")
        print("=" * 72)
        print(f"{'Metric':34} {'Full file':>16} {'30s chunks':>16}")
        print("-" * 72)
        for key, label in [
            ("segments", "Segments"),
            ("words", "Words"),
            ("adjacent_duplicate_ratio", "Adjacent duplicates"),
            ("dominant_segment_ratio", "Dominant repeated segment"),
            ("trigram_repetition_ratio", "Repeated trigrams"),
            ("repetition_score", "Repetition score"),
        ]:
            full_value = full_metrics[key]
            chunked_value = chunked_metrics[key]
            if isinstance(full_value, float):
                full_display = format_percent(full_value)
                chunked_display = format_percent(float(chunked_value))
            else:
                full_display = str(full_value)
                chunked_display = str(chunked_value)
            print(f"{label:34} {full_display:>16} {chunked_display:>16}")

        if args.reference:
            print("-" * 72)
            print(f"{'WER':34} {format_percent(float(result['full_wer'])):>16} {format_percent(float(result['chunked_wer'])):>16}")

        delta = float(result["repetition_score_delta"])
        direction = "better" if delta < 0 else "worse" if delta > 0 else "unchanged"
        print(f"\nChunked repetition score is {abs(delta) * 100:.2f} percentage points {direction}.")

    if args.fail_on_regression:
        repetition_regressed = float(result["repetition_score_delta"]) > 1e-9
        wer_regressed = args.reference is not None and float(result.get("wer_delta", 0.0)) > 1e-9
        if repetition_regressed or wer_regressed:
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
