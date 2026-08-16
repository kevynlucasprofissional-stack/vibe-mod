# Resilient 30-second transcription

Vibe can protect long transcriptions from decoder loops by splitting the media into short, independent transcription requests while keeping one continuous output timeline.

## Defaults

- Protection is enabled by default through `modelOptions.chunking_enabled`.
- Files shorter than 35 seconds keep the original single-request path.
- Principal chunk ownership is never longer than 30 seconds.
- The planner looks for a silence in the last 2 seconds before a 30-second boundary and may cut a little earlier.
- Each extraction contains 1 second of overlap on either side when media is available.
- Overlap is context only. An ownership window decides which segments are accepted.
- The Sona model remains loaded. Only transcription context is reset by issuing a new `/v1/audio/transcriptions` request for each chunk.
- The previous chunk transcript is never injected as a prompt. A user-supplied `init_prompt` is still preserved.

## Pipeline

1. Probe media duration.
2. Bypass chunking for short files or speaker diarization.
3. Detect silence positions with FFmpeg. If this analysis fails, fall back to exact 30-second boundaries.
4. Build ownership windows of at most 30 seconds and extraction windows with overlap.
5. Extract one temporary 16 kHz mono PCM WAV at a time.
6. Transcribe sequentially using the already-loaded Sona model.
7. Score the chunk for pathological repetition.
8. When repetition is detected, discard that result and retry the same ownership range as two smaller chunks.
9. Retry at 30s -> 15s -> 7.5s. At the retry floor, collapse consecutive pathological duplicates instead of allowing an infinite retry loop.
10. Convert local chunk timestamps back to the original media timeline.
11. Keep only segments owned by the current window, merge all accepted segments, and remove near-duplicate boundary segments.
12. Return one normal Vibe `Transcript`, so TXT/SRT/VTT/JSON/CSV/DOCX exporters continue to work without chunk awareness.

## Progress and cancellation

Sona reports progress per request. Vibe maps each request back to the corresponding position in the original file, so the UI sees one global 0-100% operation rather than repeated 0-100% cycles. Progress is clamped so adaptive retries do not move the UI backwards.

Only one `abort_transcribe` listener is registered for an operation, and it is explicitly removed when the command finishes. Cancellation prevents subsequent chunks from being scheduled and temporary chunks are removed after each attempt.

## Speaker diarization

Chunk protection currently falls back to the original full-file path when speaker diarization is enabled. Speaker numbers are local inference identities and cannot safely be assumed to identify the same person in separate requests. Stable timestamps and VAD remain compatible and are forwarded to every chunk.

A future diarization implementation should perform explicit cross-chunk speaker embedding matching before chunking is enabled for that mode.

## Repetition detector

The detector combines three signals and uses the strongest one:

- consecutive identical segments;
- a single segment dominating the chunk three or more times;
- repeated word trigrams across the chunk.

A score of 0.65 or greater triggers adaptive retry. The retry result is evaluated again before it is accepted.

## Validation framework

Development follows a gated loop for each change:

**Implement -> verify -> adjust -> validate -> move to the next implementation.**

The Rust unit tests cover:

- FFmpeg duration parsing;
- maximum 30-second ownership;
- silence-aware cuts;
- overlap semantics;
- global timestamp reconstruction;
- loop detection;
- healthy-text false-positive protection;
- boundary deduplication;
- adaptive 30 -> 15 -> 7.5 second retries.

Use the benchmark script to compare a known problematic file with protection disabled and enabled:

```bash
python scripts/chunking_benchmark.py full.json chunked.json
```

With a human/reference transcript:

```bash
python scripts/chunking_benchmark.py full.json chunked.json --reference reference.txt
```

For automated regression checks:

```bash
python scripts/chunking_benchmark.py full.json chunked.json --reference reference.txt --fail-on-regression
```

The comparison reports adjacent duplicates, dominant repeated segments, repeated trigrams, an aggregate repetition score, and optional word error rate (WER).

## Real-file acceptance matrix

Before declaring the feature production-ready, validate at least:

- 20-30 second audio: unchanged original path;
- 1-5 minute speech: chunked path and continuous timestamps;
- 30-60 minute speech: no repeated-loop tail;
- multi-hour audio/video: stable memory and temporary-file cleanup;
- speech crossing an exact 30-second boundary;
- long silence around a boundary;
- music/noise around a boundary;
- stable timestamps enabled;
- VAD-required model;
- CPU transcription;
- GPU transcription;
- cancellation during a chunk;
- Batch mode;
- TXT, SRT, VTT, JSON, CSV and DOCX exports;
- diarization enabled: confirmed full-file fallback.

For the original failure mode, the acceptance criterion is that 30-second protection materially lowers the repetition score without increasing WER on a representative set of long files.
