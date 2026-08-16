# Vibe structured diagnostics

Vibe writes a structured diagnostic report for the operations that are most likely to need forensic debugging: model loading, transcription, Batch, recording, HTTP downloads, yt-dlp downloads, Home LLM summarization and CLI forwarding.

The goal is not to guess a root cause automatically. The goal is to preserve enough evidence to reconstruct what the software actually did: inputs, selected strategy, stage transitions, fallbacks, failures, outputs and observable anomaly signals.

## Files and retention

Reports are stored below the app configuration directory:

```text
diagnostics/
  YYYY-MM-DD/
    <run-id>.json
    <run-id>.md
```

- JSON is the primary machine-readable artifact to share for debugging.
- Markdown is the human-readable summary written when a run is finalized.
- A JSON snapshot is refreshed throughout the run, so a hard failure can still leave partial evidence.
- Structured reports are retained for 30 days.
- Raw tracing logs are retained for 14 days and are referenced as complementary evidence.

The Settings > Advanced screen can copy the latest JSON report, reveal the latest report, or open the diagnostics folder.

## What a report contains

Every report has a `run_id` and records:

- Vibe version and commit hash;
- operating system, architecture and AVX2 support;
- sanitized operation context;
- start/end time and elapsed time;
- final outcome (`succeeded`, `failed`, `partial`, `aborted`, `crashed`, etc.);
- ordered event timeline with stage/category/severity;
- Batch child-item states when applicable;
- anomaly signals;
- final result summary;
- references to complementary raw evidence.

### Transcription

A transcription run can record:

- input file metadata;
- selected Sona engine;
- model path (home directory redacted);
- requested GPU and whether CPU fallback occurred;
- chunking/diarization/stable-timestamp policy decisions;
- media-duration probe and fallback decisions;
- Whisper chunk plan;
- each chunk start/completion;
- FFmpeg extraction/cleanup errors;
- merge counts;
- Sona stream failures and recent Sona stderr;
- cancellation;
- structural quality signals.

### Batch

One parent report represents the whole batch. Each file is tracked as `queued`, `running`, `succeeded`, `failed`, `aborted` or `skipped`.

The timeline also distinguishes:

- model loading;
- transcription;
- optional LLM summarization;
- each requested export format;
- summary export;
- item-level failures.

This is important because an export error must not be mistaken for a transcription/model failure.

### Recording

Recording diagnostics capture:

- selected devices and negotiated stream configuration;
- WAV sources and sample counts;
- source merge/fallback decision;
- normalization attempt/fallback;
- final-storage move/copy fallback;
- cleanup warnings;
- final file existence/metadata.

### Downloads

HTTP/model downloads record start/outcome, destination and final size. yt-dlp records process lifecycle, sanitized URL, exit code, cancellation and recent stderr.

### Model loading

Model load has its own report so failures that happen before a transcription are still reconstructible. It records Sona restarts, GPU attempt, CPU fallback, engine metadata and recent stderr.

### CLI

The Vibe CLI forwards directly to the bundled Sona process. The report therefore contains process-level evidence (command family, argument count, elapsed time, exit status and recent stderr) rather than pretending Vibe can observe Sona decoder internals.

## Structural quality signals are not accuracy scores

Without a human reference transcript, Vibe does **not** claim that a transcript is accurate or inaccurate.

It can flag observable conditions such as:

- empty transcript;
- invalid or backwards timestamps;
- large temporal gaps;
- very low timestamp coverage;
- many adjacent identical segments;
- one segment dominating a large share of the output.

These signals can indicate a problem, but they may also reflect legitimate silence or deliberate repeated speech. They are evidence for investigation, not destructive rules and not WER/CER.

When a reference transcript exists, use `scripts/chunking_benchmark.py --reference ...` for reference-backed WER/CER and edit-component comparison.

## Privacy and sharing

The report is designed to be shareable but should still be reviewed before publishing publicly.

Automatic protections include:

- fields whose names resemble API keys, tokens, passwords, authorization headers or secrets are replaced with `<redacted>`;
- the current user home directory is replaced by `$HOME` in paths;
- diagnostic URL summaries for download operations remove credentials, query strings and fragments;
- audio bytes are never embedded;
- long prompt-like fields are truncated.

For debugging with ChatGPT, prefer sharing the latest `.json` report. The `.md` file is useful for a quick human overview.

## Coverage and known limits

Structured run reporting currently covers the major long-running user workflows listed above. Normal tracing still covers lower-level code that is not attached to an operation report.

Important limits:

1. The local HTTP API exposes Sona directly. After an external client begins sending requests directly to Sona, Vibe can observe the sidecar lifecycle/stderr but cannot correlate every HTTP request with a Vibe `run_id` without adding diagnostics inside Sona itself.
2. Native crashes are best-effort. The crash/panic hooks attempt to finalize active reports, but no filesystem write can be guaranteed after every class of process/OS failure.
3. Frontend/browser crashes may prevent the normal operation-finalization call; incremental JSON snapshots exist specifically to preserve the last known state.
4. Reports describe the runtime Vibe can observe. They do not prove transcription accuracy without ground truth.

## Minimum debugging package

When reporting a failure, provide:

1. the latest structured JSON diagnostic;
2. the matching raw log if deeper low-level context is needed;
3. the input audio or a reproducible sample when licensing/privacy allows it;
4. a human reference transcript for accuracy claims when available;
5. the exact expected behavior versus observed behavior.
