# Audited long-file transcription protection

This branch protects long **Whisper** transcriptions from context-propagated repetition loops by sending independent requests to Sona while preserving a continuous output timeline.

The design in this document is the result of an audit of the first PR implementation. It intentionally removes mechanisms that were not proven safe.

## What the audit established

### Why separate requests help Whisper

Sona v0.3.5 keeps the model loaded but each Whisper request begins with fresh transcription context. Within one long Whisper request, however, previous-window text can condition later windows. That is the propagation path this feature is intended to break.

For that reason, the desktop flow limits external Whisper requests to **at most 30 seconds**.

### Why this is not applied to every engine

Sona v0.3.5 already performs native VAD chunking for Nemotron and Parakeet, with a 30-second maximum chunk duration. Adding a second Vibe-side cut layer was not justified by the audit.

The backend records Sona's reported model engine when a model is loaded:

- `whisper`: external desktop protection may run;
- `nemotron`: use Sona-native chunking;
- `parakeet`: use Sona-native chunking;
- unknown/custom metadata: treated conservatively as Whisper-compatible.

## Current Whisper pipeline

For a Whisper model with protection enabled and diarization disabled:

1. Probe the media duration.
2. Files of 30 seconds or less use the normal single-request path.
3. Longer files are planned as fixed requests of at most 30 seconds.
4. Neighboring requests start 28 seconds apart, creating a 2-second shared audio region.
5. Each request is extracted as 16 kHz mono PCM WAV.
6. Sona remains loaded; each chunk is sent as an independent `/v1/audio/transcriptions` request.
7. Chunk-local timestamps are shifted back to the source-media timeline.
8. All overlap output is preserved initially.
9. Two segments are collapsed only when they have:
   - exactly equal normalized text;
   - the same speaker value;
   - a real positive temporal overlap.
10. Ambiguous boundary variants are kept rather than guessed away.
11. The merged result is returned as one normal Vibe `Transcript`.

## What was deliberately removed

The first implementation included several additional mechanisms. The audit did not find sufficient evidence to keep them in the production path.

### Midpoint ownership

Removed because two neighboring requests can segment a boundary phrase differently and both midpoint tests can reject it, deleting real speech.

### Whole-file silence detection

Removed because it is not necessary for complete temporal coverage. Fixed overlapping windows cover the whole file without requiring thresholds for dB level, silence duration or lookback distance.

Silence-aware cutting may still be useful as a future quality optimization, but it needs real-model evidence before returning.

### Text-based repetition detector and adaptive retry

Removed from production. Legitimate repeated speech can look identical to a decoder loop when only transcript text is examined.

The earlier detector could flag legitimate repeated phrases and the retry-floor sanitizer could delete real occurrences. Repetition metrics remain useful for diagnostics, but they are not allowed to delete transcript content.

### Fuzzy boundary deduplication

Removed. Similar text near a boundary is not enough evidence that one copy is false. The merge now only collapses exact normalized text with actual time overlap.

## Diarization

External Whisper chunking is **not used when diarization is enabled**.

In Sona v0.3.5, diarization runs on the audio in each request and exposes request-local speaker IDs. The API does not expose a global speaker identity/embedding that Vibe can safely use to reconcile `Speaker 1` across independent requests.

The desktop UI states this limitation explicitly. A diarized Whisper run therefore uses the normal full-file path and should not be described as protected by this feature.

## Product scope

This implementation lives in the Vibe desktop `transcribe` orchestrator.

Covered desktop flows:

- Home file transcription;
- files produced by recording and then sent through Home transcription;
- downloaded/link media that enters the Home transcription flow;
- Batch transcription.

Not covered by this Vibe-side orchestrator:

- CLI mode, which forwards arguments directly to the bundled Sona binary;
- the local HTTP API, which exposes Sona directly;
- agent skills that call the Sona HTTP API directly.

A product-wide version of this behavior would be cleaner if Sona exposed an explicit Whisper option equivalent to disabling previous-window text conditioning. Sona v0.3.5 does not expose that option.

## Benchmark policy

`scripts/chunking_benchmark.py` no longer treats lower repetition as proof of better transcription.

Without a reference transcript it reports only diagnostics and returns:

`quality_verdict = inconclusive_without_reference`

It reports:

- word/segment counts;
- adjacent duplicates;
- dominant repeated segments;
- repeated trigrams;
- aggregate repetition score;
- timestamp validity;
- ordering;
- first/last timestamps;
- temporal span and maximum inter-segment gap.

With a human/reference transcript it additionally reports:

- WER;
- CER;
- word substitutions;
- word insertions;
- word deletions/omissions.

`--fail-on-regression` requires `--reference`. The gate fails when the protected transcript has worse WER/CER, more word omissions, more word insertions, or structurally invalid timestamps.

Examples:

```bash
python scripts/chunking_benchmark.py full.json chunked.json
```

Diagnostic only; no quality winner is declared.

```bash
python scripts/chunking_benchmark.py full.json chunked.json --reference reference.txt
```

Reference-backed comparison.

```bash
python scripts/chunking_benchmark.py full.json chunked.json --reference reference.txt --fail-on-regression
```

Reference-backed regression gate.

## Parameters that remain hypotheses

The following values are implementation defaults, not empirically optimized conclusions:

- request ceiling: 30 seconds has a technical basis in Whisper windowing and context isolation;
- shared boundary audio: 2 seconds is retained as a conservative overlap, but its optimal value is not yet established.

Do not claim that 2 seconds is optimal until real-model experiments compare alternatives.

## Validation completed without a real model runtime

The audit validated:

- independent Sona requests reset relevant decoder state;
- Whisper long-request context can propagate between internal windows;
- Nemotron/Parakeet already have Sona-native chunking;
- fixed 30-second windows with 2-second overlap cover arbitrary durations without gaps;
- generated FFmpeg cuts have the requested lengths;
- conservative merge preserves the reproduced boundary-loss case;
- legitimate sequential repeated phrases are not removed by the merge;
- a no-reference benchmark cannot approve an empty transcript as a quality improvement.

## Remaining empirical gates

This branch must **not** be called production-ready based only on the checks above.

Still required on a machine with the real app/model runtime:

1. Rust formatter/build/check/clippy/test.
2. Frontend build/typecheck.
3. Real Vibe → FFmpeg → Sona → model → merge → export integration.
4. A/B/C accuracy comparison using a file that reproduces the original loop:
   - A: original full-file behavior;
   - B: first PR implementation before audit corrections;
   - C: audited implementation.
5. A reference-backed quality comparison when possible.
6. Boundary-focused listening/ground-truth checks.
7. CPU and GPU runs.
8. Runtime and resource measurements for long files.
9. Cancellation while an FFmpeg chunk extraction is active.
10. Export checks for TXT, JSON, SRT, VTT, CSV and DOCX.

Until those gates pass, the correct release status is **not ready for merge**.
