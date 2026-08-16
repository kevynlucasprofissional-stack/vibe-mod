use crate::diagnostics::{analyze_transcript, input_file_metadata, DiagnosticsState};
use crate::error::LogError;
use crate::setup::SonaState;
use crate::sona::SonaEvent;
use crate::transcript::{Segment, Transcript};
use eyre::Result;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicI64, Ordering},
    Arc,
};
use tauri::{Emitter, Listener, State};
use tokio::sync::Mutex;

use super::chunking::{
    extract_chunk, globalize_segments, merge_segments, plan_chunks, probe_duration_seconds,
    CHUNKING_MIN_DURATION_SECONDS, DEFAULT_CHUNK_SECONDS,
};
use super::{ui::set_progress_bar, CommandError};

#[allow(dead_code)]
#[derive(Deserialize, Serialize, Clone)]
pub struct FfmpegOptions {
    pub normalize_loudness: bool,
    pub custom_command: Option<String>,
}

impl Default for FfmpegOptions {
    fn default() -> Self {
        Self {
            normalize_loudness: true,
            custom_command: None,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct TranscribeOptions {
    pub path: String,
    pub lang: Option<String>,
    pub verbose: Option<bool>,
    pub n_threads: Option<i32>,
    pub init_prompt: Option<String>,
    pub temperature: Option<f32>,
    pub translate: Option<bool>,
    pub max_text_ctx: Option<i32>,
    pub word_timestamps: Option<bool>,
    pub max_sentence_len: Option<i32>,
    pub sampling_strategy: Option<String>,
    pub best_of: Option<i32>,
    pub beam_size: Option<i32>,
    pub diarize_model: Option<String>,
    pub stable_timestamps: Option<bool>,
    pub vad_model: Option<String>,
    pub chunking_enabled: Option<bool>,
    pub diagnostic_run_id: Option<String>,
    pub diagnostic_item_id: Option<String>,
    pub diagnostic_source: Option<String>,
}

#[derive(Clone, Copy)]
enum ProgressMode {
    Direct,
    Chunked {
        chunk_start: f64,
        chunk_duration: f64,
        media_duration: f64,
    },
}

struct TranscriptionExecution {
    segments: Vec<Segment>,
    media_duration_seconds: Option<f64>,
    mode: &'static str,
    chunks_processed: usize,
}

#[tauri::command]
pub async fn transcribe(
    app_handle: tauri::AppHandle,
    options: TranscribeOptions,
    sona_state: State<'_, Mutex<SonaState>>,
    diagnostics: State<'_, DiagnosticsState>,
) -> Result<Transcript, CommandError> {
    let audio_path = PathBuf::from(&options.path);
    let owns_diagnostic_run = options.diagnostic_run_id.is_none();
    let source = options.diagnostic_source.as_deref().unwrap_or("transcription");
    let item_id = options
        .diagnostic_item_id
        .clone()
        .unwrap_or_else(|| audio_path.file_name().unwrap_or_default().to_string_lossy().to_string());
    let item_label = audio_path.file_name().unwrap_or_default().to_string_lossy().to_string();

    let diagnostic_run_id = if let Some(run_id) = options.diagnostic_run_id.clone() {
        Some(run_id)
    } else {
        let context = json!({
            "operation": "transcription",
            "input": input_file_metadata(&audio_path),
            "options": serde_json::to_value(&options).unwrap_or_else(|_| json!({"serialization_error": true})),
        });
        match diagnostics.start_run(source, context) {
            Ok(run_id) => Some(run_id),
            Err(error) => {
                tracing::error!("failed to start diagnostic run: {error:?}");
                None
            }
        }
    };

    if let Some(run_id) = diagnostic_run_id.as_deref() {
        diagnostics
            .upsert_item(
                run_id,
                &item_id,
                &item_label,
                "running",
                json!({ "input": input_file_metadata(&audio_path) }),
                None,
            )
            .log_error();
        diag_event(
            &diagnostics,
            Some(run_id),
            "info",
            "input",
            "transcription.requested",
            "Transcription request accepted by the Vibe orchestrator",
            json!({ "input": input_file_metadata(&audio_path) }),
        );
    }

    if let Err(error) = validate_audio_path(&audio_path, &options.path) {
        finalize_failure(
            &diagnostics,
            diagnostic_run_id.as_deref(),
            owns_diagnostic_run,
            &item_id,
            &item_label,
            &error,
            "input.validation",
        );
        return Err(error);
    }

    let (client, base_url, model_engine, loaded_model_path, gpu_device, gpu_fallback) = {
        let state = sona_state.lock().await;
        let Some(process) = state.process.as_ref() else {
            let error = CommandError {
                code: "no_model".to_string(),
                message: "Please load model first".to_string(),
            };
            finalize_failure(
                &diagnostics,
                diagnostic_run_id.as_deref(),
                owns_diagnostic_run,
                &item_id,
                &item_label,
                &error,
                "model.availability",
            );
            return Err(error);
        };
        (
            process.client(),
            process.base_url(),
            state.model_engine.clone(),
            state.loaded_model_path.clone(),
            state.gpu_device,
            state.gpu_fallback,
        )
    };

    let abort_atomic = Arc::new(AtomicBool::new(false));
    let abort_atomic_c = abort_atomic.clone();
    let app_handle_c = app_handle.clone();
    let listener_id = app_handle.listen("abort_transcribe", move |_| {
        let _ = set_progress_bar(&app_handle_c, None);
        abort_atomic_c.store(true, Ordering::Relaxed);
    });

    let started_at = std::time::Instant::now();
    diag_event(
        &diagnostics,
        diagnostic_run_id.as_deref(),
        "info",
        "model",
        "transcription.model_ready",
        "Sona process and model are available",
        json!({
            "engine": model_engine.as_deref(),
            "model_path": loaded_model_path.as_deref(),
            "gpu_device": gpu_device,
            "gpu_fallback": gpu_fallback,
            "base_url": base_url.as_str(),
        }),
    );

    let result = transcribe_inner(
        &app_handle,
        &client,
        &base_url,
        &audio_path,
        &options,
        &abort_atomic,
        model_engine.as_deref(),
        &diagnostics,
        diagnostic_run_id.as_deref(),
    )
    .await;

    app_handle.unlisten(listener_id);
    let _ = set_progress_bar(&app_handle, None);

    if abort_atomic.load(Ordering::Relaxed) {
        let error = aborted_error();
        diag_event(
            &diagnostics,
            diagnostic_run_id.as_deref(),
            "warning",
            "lifecycle",
            "transcription.aborted",
            "Transcription was aborted by the user",
            json!({ "elapsed_ms": started_at.elapsed().as_millis() }),
        );
        if let Some(run_id) = diagnostic_run_id.as_deref() {
            diagnostics
                .upsert_item(
                    run_id,
                    &item_id,
                    &item_label,
                    "aborted",
                    json!({ "elapsed_ms": started_at.elapsed().as_millis() }),
                    Some(command_error_value(&error)),
                )
                .log_error();
            if owns_diagnostic_run {
                diagnostics
                    .finish_run(
                        run_id,
                        "aborted",
                        json!({ "error": command_error_value(&error), "elapsed_ms": started_at.elapsed().as_millis() }),
                    )
                    .log_error();
            }
        }
        return Err(error);
    }

    match result {
        Ok(execution) => {
            let elapsed = started_at.elapsed();
            let (quality_metrics, anomalies) = analyze_transcript(&execution.segments, execution.media_duration_seconds);
            if let Some(run_id) = diagnostic_run_id.as_deref() {
                for anomaly in anomalies {
                    diagnostics.add_anomaly(run_id, anomaly).log_error();
                }
                let result_data = json!({
                    "mode": execution.mode,
                    "chunks_processed": execution.chunks_processed,
                    "processing_time_ms": elapsed.as_millis(),
                    "quality_signals": quality_metrics,
                });
                diagnostics
                    .upsert_item(run_id, &item_id, &item_label, "succeeded", result_data.clone(), None)
                    .log_error();
                diag_event(
                    &diagnostics,
                    Some(run_id),
                    "info",
                    "result",
                    "transcription.completed",
                    "Transcription completed and structural quality signals were evaluated",
                    result_data.clone(),
                );
                if owns_diagnostic_run {
                    diagnostics.finish_run(run_id, "succeeded", result_data).log_error();
                }
            }
            Ok(Transcript {
                processing_time_sec: elapsed.as_secs(),
                segments: execution.segments,
            })
        }
        Err(error) => {
            if let Some(run_id) = diagnostic_run_id.as_deref() {
                let recent_stderr = {
                    let state = sona_state.lock().await;
                    state
                        .process
                        .as_ref()
                        .map(crate::sona::SonaProcess::recent_stderr)
                        .unwrap_or_default()
                };
                if !recent_stderr.is_empty() {
                    diag_event(
                        &diagnostics,
                        Some(run_id),
                        "error",
                        "sona",
                        "transcription.sona_stderr",
                        "Recent Sona stderr captured after transcription failure",
                        json!({ "recent_stderr": recent_stderr }),
                    );
                }
            }
            finalize_failure(
                &diagnostics,
                diagnostic_run_id.as_deref(),
                owns_diagnostic_run,
                &item_id,
                &item_label,
                &error,
                "transcription.failed",
            );
            Err(error)
        }
    }
}

fn aborted_error() -> CommandError {
    CommandError {
        code: "aborted".to_string(),
        message: "Transcription aborted by user".to_string(),
    }
}

fn validate_audio_path(audio_path: &Path, original: &str) -> Result<(), CommandError> {
    if !audio_path.exists() {
        return Err(CommandError {
            code: "invalid_request".to_string(),
            message: format!("Audio file not found: {original}"),
        });
    }
    if !audio_path.is_file() {
        return Err(CommandError {
            code: "invalid_request".to_string(),
            message: format!("Path is not a file: {original}"),
        });
    }
    Ok(())
}

fn engine_uses_external_chunking(engine: Option<&str>) -> bool {
    engine.is_none_or(|engine| engine.eq_ignore_ascii_case("whisper"))
}

fn should_use_external_chunking(enabled: bool, engine: Option<&str>, diarization_enabled: bool) -> bool {
    enabled && !diarization_enabled && engine_uses_external_chunking(engine)
}

async fn transcribe_inner(
    app_handle: &tauri::AppHandle,
    client: &reqwest::Client,
    base_url: &str,
    audio_path: &Path,
    options: &TranscribeOptions,
    abort_atomic: &AtomicBool,
    model_engine: Option<&str>,
    diagnostics: &DiagnosticsState,
    diagnostic_run_id: Option<&str>,
) -> Result<TranscriptionExecution, CommandError> {
    let chunking_enabled = options.chunking_enabled.unwrap_or(true);
    let diarization_enabled = options
        .diarize_model
        .as_deref()
        .is_some_and(|model| !model.trim().is_empty());
    let external_chunking = should_use_external_chunking(chunking_enabled, model_engine, diarization_enabled);

    diag_event(
        diagnostics,
        diagnostic_run_id,
        "info",
        "decision",
        "transcription.policy",
        "Transcription strategy selected",
        json!({
            "engine": model_engine.unwrap_or("unknown"),
            "chunking_requested": chunking_enabled,
            "diarization_enabled": diarization_enabled,
            "external_whisper_chunking": external_chunking,
            "stable_timestamps": options.stable_timestamps.unwrap_or(false),
            "vad_model_present": options.vad_model.as_ref().is_some_and(|value| !value.is_empty()),
        }),
    );

    if chunking_enabled && diarization_enabled {
        tracing::warn!("Whisper long-file protection is disabled for this run because cross-chunk speaker identity is not safe");
        diag_event(
            diagnostics,
            diagnostic_run_id,
            "warning",
            "decision",
            "transcription.chunking_bypassed_diarization",
            "External Whisper chunking was bypassed because cross-request speaker identity is not safe",
            json!({}),
        );
    } else if chunking_enabled && !engine_uses_external_chunking(model_engine) {
        tracing::debug!(
            engine = model_engine.unwrap_or("unknown"),
            "external chunking bypassed because this engine uses Sona-native chunking"
        );
        diag_event(
            diagnostics,
            diagnostic_run_id,
            "info",
            "decision",
            "transcription.chunking_bypassed_engine",
            "External chunking was bypassed because this engine uses Sona-native chunking",
            json!({ "engine": model_engine.unwrap_or("unknown") }),
        );
    }

    if !external_chunking {
        let segments = transcribe_stream_collect(
            app_handle,
            client,
            base_url,
            options,
            abort_atomic,
            ProgressMode::Direct,
            None,
        )
        .await?;
        return Ok(TranscriptionExecution {
            segments,
            media_duration_seconds: None,
            mode: "direct",
            chunks_processed: 1,
        });
    }

    diag_event(
        diagnostics,
        diagnostic_run_id,
        "info",
        "ffmpeg",
        "media.probe_started",
        "Probing media duration before Whisper chunk planning",
        json!({}),
    );
    let media_duration = match probe_duration_seconds(audio_path, abort_atomic).await {
        Ok(duration) => {
            diag_event(
                diagnostics,
                diagnostic_run_id,
                "info",
                "ffmpeg",
                "media.probe_completed",
                "Media duration probe completed",
                json!({ "duration_seconds": duration }),
            );
            duration
        }
        Err(error) if abort_atomic.load(Ordering::Relaxed) => {
            tracing::debug!("Whisper duration probe aborted: {error:?}");
            return Ok(TranscriptionExecution {
                segments: Vec::new(),
                media_duration_seconds: None,
                mode: "aborted_during_probe",
                chunks_processed: 0,
            });
        }
        Err(error) => {
            tracing::warn!("unable to probe duration for Whisper chunking, using normal transcription: {error:?}");
            diag_event(
                diagnostics,
                diagnostic_run_id,
                "warning",
                "fallback",
                "media.probe_failed",
                "Duration probe failed; falling back to one normal Sona request",
                json!({ "error": error.to_string() }),
            );
            let segments = transcribe_stream_collect(
                app_handle,
                client,
                base_url,
                options,
                abort_atomic,
                ProgressMode::Direct,
                None,
            )
            .await?;
            return Ok(TranscriptionExecution {
                segments,
                media_duration_seconds: None,
                mode: "direct_after_probe_failure",
                chunks_processed: 1,
            });
        }
    };

    if media_duration <= CHUNKING_MIN_DURATION_SECONDS {
        diag_event(
            diagnostics,
            diagnostic_run_id,
            "info",
            "decision",
            "transcription.short_file_direct",
            "Media is at or below the chunking threshold; using one Sona request",
            json!({ "duration_seconds": media_duration, "threshold_seconds": CHUNKING_MIN_DURATION_SECONDS }),
        );
        let segments = transcribe_stream_collect(
            app_handle,
            client,
            base_url,
            options,
            abort_atomic,
            ProgressMode::Direct,
            None,
        )
        .await?;
        return Ok(TranscriptionExecution {
            segments,
            media_duration_seconds: Some(media_duration),
            mode: "direct_short_file",
            chunks_processed: 1,
        });
    }

    let (segments, chunks_processed) = transcribe_chunked(
        app_handle,
        client,
        base_url,
        audio_path,
        options,
        abort_atomic,
        media_duration,
        diagnostics,
        diagnostic_run_id,
    )
    .await?;
    Ok(TranscriptionExecution {
        segments,
        media_duration_seconds: Some(media_duration),
        mode: "whisper_fixed_chunks",
        chunks_processed,
    })
}

async fn transcribe_chunked(
    app_handle: &tauri::AppHandle,
    client: &reqwest::Client,
    base_url: &str,
    audio_path: &Path,
    options: &TranscribeOptions,
    abort_atomic: &AtomicBool,
    media_duration: f64,
    diagnostics: &DiagnosticsState,
    diagnostic_run_id: Option<&str>,
) -> Result<(Vec<Segment>, usize), CommandError> {
    if abort_atomic.load(Ordering::Relaxed) {
        return Ok((Vec::new(), 0));
    }

    let windows = plan_chunks(media_duration);
    tracing::info!(
        "Whisper fixed-window protection enabled: duration={:.2}s chunks={} request_ceiling={}s",
        media_duration,
        windows.len(),
        DEFAULT_CHUNK_SECONDS
    );
    diag_event(
        diagnostics,
        diagnostic_run_id,
        "info",
        "decision",
        "chunks.planned",
        "Whisper fixed overlapping chunks planned",
        json!({
            "media_duration_seconds": media_duration,
            "chunk_count": windows.len(),
            "request_ceiling_seconds": DEFAULT_CHUNK_SECONDS,
        }),
    );

    let mut accepted_segments = Vec::new();
    let reported_progress = AtomicI64::new(0);
    let mut chunks_processed = 0usize;

    for (index, window) in windows.into_iter().enumerate() {
        if abort_atomic.load(Ordering::Relaxed) {
            tracing::debug!("chunked transcription aborted by user");
            break;
        }
        let chunk_number = index + 1;
        diag_event(
            diagnostics,
            diagnostic_run_id,
            "info",
            "chunk",
            "chunk.started",
            "Starting chunk extraction and transcription",
            json!({
                "chunk": chunk_number,
                "start_seconds": window.start,
                "end_seconds": window.end,
                "duration_seconds": window.duration(),
            }),
        );

        let chunk_path = match extract_chunk(audio_path, window, abort_atomic).await {
            Ok(path) => path,
            Err(error) if abort_atomic.load(Ordering::Relaxed) => {
                tracing::debug!("active FFmpeg chunk extraction aborted: {error:?}");
                diag_event(
                    diagnostics,
                    diagnostic_run_id,
                    "warning",
                    "ffmpeg",
                    "chunk.extraction_aborted",
                    "Active FFmpeg chunk extraction was aborted",
                    json!({ "chunk": chunk_number, "error": error.to_string() }),
                );
                break;
            }
            Err(error) => {
                diag_event(
                    diagnostics,
                    diagnostic_run_id,
                    "error",
                    "ffmpeg",
                    "chunk.extraction_failed",
                    "FFmpeg failed to extract a transcription chunk",
                    json!({ "chunk": chunk_number, "error": error.to_string() }),
                );
                return Err(CommandError::from(error));
            }
        };

        let mut chunk_options = options.clone();
        chunk_options.path = chunk_path.to_string_lossy().to_string();
        chunk_options.chunking_enabled = Some(false);

        let transcription = transcribe_stream_collect(
            app_handle,
            client,
            base_url,
            &chunk_options,
            abort_atomic,
            ProgressMode::Chunked {
                chunk_start: window.start,
                chunk_duration: window.duration(),
                media_duration,
            },
            Some(&reported_progress),
        )
        .await;

        if let Err(error) = std::fs::remove_file(&chunk_path) {
            tracing::debug!("failed to remove temporary chunk {}: {error}", chunk_path.display());
            diag_event(
                diagnostics,
                diagnostic_run_id,
                "warning",
                "filesystem",
                "chunk.temp_cleanup_failed",
                "Temporary chunk could not be removed",
                json!({ "chunk": chunk_number, "path": chunk_path, "error": error.to_string() }),
            );
        }

        let local_segments = transcription?;
        if abort_atomic.load(Ordering::Relaxed) {
            break;
        }

        let global_segments = globalize_segments(local_segments, window, media_duration);
        let segment_count = global_segments.len();
        for segment in &global_segments {
            app_handle
                .emit_to("main", "new_segment", segment.clone())
                .log_error();
        }
        accepted_segments.extend(global_segments);
        chunks_processed += 1;
        diag_event(
            diagnostics,
            diagnostic_run_id,
            "info",
            "chunk",
            "chunk.completed",
            "Chunk transcription completed",
            json!({ "chunk": chunk_number, "segments": segment_count }),
        );
    }

    if !abort_atomic.load(Ordering::Relaxed) {
        let _ = set_progress_bar(app_handle, Some(100.0));
    }

    let before_merge = accepted_segments.len();
    let merged = merge_segments(accepted_segments);
    diag_event(
        diagnostics,
        diagnostic_run_id,
        "info",
        "merge",
        "chunks.merged",
        "Chunk outputs were reconciled on the global timeline",
        json!({
            "segments_before_merge": before_merge,
            "segments_after_merge": merged.len(),
            "duplicates_collapsed": before_merge.saturating_sub(merged.len()),
        }),
    );
    Ok((merged, chunks_processed))
}

async fn transcribe_stream_collect(
    app_handle: &tauri::AppHandle,
    client: &reqwest::Client,
    base_url: &str,
    options: &TranscribeOptions,
    abort_atomic: &AtomicBool,
    progress_mode: ProgressMode,
    reported_progress: Option<&AtomicI64>,
) -> Result<Vec<Segment>, CommandError> {
    let stream = crate::sona::SonaProcess::transcribe_stream(client, base_url, options)
        .await
        .map_err(map_sona_error)?;
    tokio::pin!(stream);

    let mut segments = Vec::new();
    let mut completed = false;

    while let Some(event_result) = stream.next().await {
        if abort_atomic.load(Ordering::Relaxed) {
            tracing::debug!("transcription aborted by user");
            break;
        }

        match event_result {
            Ok(event) => match event {
                SonaEvent::Progress { progress } => {
                    update_progress(app_handle, progress, progress_mode, reported_progress);
                }
                SonaEvent::Segment {
                    start,
                    end,
                    text,
                    speaker,
                } => {
                    let segment = Segment {
                        start: (start * 100.0) as i64,
                        stop: (end * 100.0) as i64,
                        text,
                        speaker,
                    };
                    if matches!(progress_mode, ProgressMode::Direct) {
                        app_handle
                            .emit_to("main", "new_segment", segment.clone())
                            .log_error();
                    }
                    segments.push(segment);
                }
                SonaEvent::Result { .. } => {
                    completed = true;
                }
                SonaEvent::Error { code, message } => {
                    tracing::error!("sona transcription error: {}", message);
                    return Err(CommandError {
                        code: code.unwrap_or_else(|| "internal_error".to_string()),
                        message,
                    });
                }
            },
            Err(error) => {
                tracing::error!("stream error: {:?}", error);
                return Err(CommandError::from(error));
            }
        }
    }

    if !abort_atomic.load(Ordering::Relaxed) && !completed {
        return Err(CommandError {
            code: "internal_error".to_string(),
            message: "Sona transcription stream ended before completion".to_string(),
        });
    }

    Ok(segments)
}

fn update_progress(
    app_handle: &tauri::AppHandle,
    progress: i32,
    mode: ProgressMode,
    reported_progress: Option<&AtomicI64>,
) {
    let progress = progress.clamp(0, 100) as f64;
    let mapped = match mode {
        ProgressMode::Direct => progress,
        ProgressMode::Chunked {
            chunk_start,
            chunk_duration,
            media_duration,
        } => ((chunk_start + chunk_duration * (progress / 100.0)) / media_duration * 100.0)
            .clamp(0.0, 99.9),
    };

    if let Some(reported) = reported_progress {
        let candidate = (mapped * 1000.0).round() as i64;
        let previous = reported.fetch_max(candidate, Ordering::Relaxed);
        if candidate < previous {
            return;
        }
    }

    let _ = set_progress_bar(app_handle, Some(mapped));
}

fn diag_event(
    diagnostics: &DiagnosticsState,
    run_id: Option<&str>,
    severity: &str,
    category: &str,
    stage: &str,
    message: &str,
    data: Value,
) {
    if let Some(run_id) = run_id {
        diagnostics
            .record_event(run_id, severity, category, stage, message, data)
            .log_error();
    }
}

fn finalize_failure(
    diagnostics: &DiagnosticsState,
    run_id: Option<&str>,
    owns_run: bool,
    item_id: &str,
    item_label: &str,
    error: &CommandError,
    stage: &str,
) {
    let Some(run_id) = run_id else {
        return;
    };
    let error_value = command_error_value(error);
    diagnostics
        .record_event(
            run_id,
            "error",
            "failure",
            stage,
            "Transcription failed",
            error_value.clone(),
        )
        .log_error();
    diagnostics
        .upsert_item(
            run_id,
            item_id,
            item_label,
            "failed",
            json!({}),
            Some(error_value.clone()),
        )
        .log_error();
    if owns_run {
        diagnostics
            .finish_run(run_id, "failed", json!({ "error": error_value }))
            .log_error();
    }
}

fn command_error_value(error: &CommandError) -> Value {
    json!({ "code": error.code.as_str(), "message": error.message.as_str() })
}

fn map_sona_error(error: eyre::Report) -> CommandError {
    if let Some(api_error) = error.downcast_ref::<crate::sona::SonaApiError>() {
        CommandError {
            code: api_error.code.clone(),
            message: api_error.message.clone(),
        }
    } else {
        CommandError::from(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_chunking_targets_whisper_and_unknown_custom_models() {
        assert!(engine_uses_external_chunking(Some("whisper")));
        assert!(engine_uses_external_chunking(Some("WHISPER")));
        assert!(engine_uses_external_chunking(None));
        assert!(!engine_uses_external_chunking(Some("nemotron")));
        assert!(!engine_uses_external_chunking(Some("parakeet")));
    }

    #[test]
    fn diarization_always_bypasses_external_chunking() {
        assert!(!should_use_external_chunking(true, Some("whisper"), true));
        assert!(should_use_external_chunking(true, Some("whisper"), false));
        assert!(!should_use_external_chunking(false, Some("whisper"), false));
    }

    #[test]
    fn abort_is_a_distinct_command_error() {
        let error = aborted_error();
        assert_eq!(error.code, "aborted");
    }
}
