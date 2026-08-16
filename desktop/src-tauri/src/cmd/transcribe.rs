use crate::error::LogError;
use crate::setup::SonaState;
use crate::sona::SonaEvent;
use crate::transcript::{Segment, Transcript};
use eyre::Result;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
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

#[tauri::command]
pub async fn transcribe(
    app_handle: tauri::AppHandle,
    options: TranscribeOptions,
    sona_state: State<'_, Mutex<SonaState>>,
) -> Result<Transcript, CommandError> {
    let audio_path = PathBuf::from(&options.path);
    validate_audio_path(&audio_path, &options.path)?;

    let (client, base_url, model_engine) = {
        let state = sona_state.lock().await;
        let process = state.process.as_ref().ok_or_else(|| CommandError {
            code: "no_model".to_string(),
            message: "Please load model first".to_string(),
        })?;
        (process.client(), process.base_url(), state.model_engine.clone())
    };

    let abort_atomic = Arc::new(AtomicBool::new(false));
    let abort_atomic_c = abort_atomic.clone();
    let app_handle_c = app_handle.clone();
    let listener_id = app_handle.listen("abort_transcribe", move |_| {
        let _ = set_progress_bar(&app_handle_c, None);
        abort_atomic_c.store(true, Ordering::Relaxed);
    });

    let started_at = std::time::Instant::now();
    let result = transcribe_inner(
        &app_handle,
        &client,
        &base_url,
        &audio_path,
        &options,
        &abort_atomic,
        model_engine.as_deref(),
    )
    .await;

    app_handle.unlisten(listener_id);
    let _ = set_progress_bar(&app_handle, None);

    result.map(|segments| Transcript {
        processing_time_sec: started_at.elapsed().as_secs(),
        segments,
    })
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
) -> Result<Vec<Segment>, CommandError> {
    let chunking_enabled = options.chunking_enabled.unwrap_or(true);
    let diarization_enabled = options
        .diarize_model
        .as_deref()
        .is_some_and(|model| !model.trim().is_empty());

    if chunking_enabled && diarization_enabled {
        tracing::warn!(
            "Whisper long-file protection is disabled for this run because cross-chunk speaker identity is not safe"
        );
    } else if chunking_enabled && !engine_uses_external_chunking(model_engine) {
        tracing::debug!(
            engine = model_engine.unwrap_or("unknown"),
            "external chunking bypassed because this engine uses Sona-native chunking"
        );
    }

    if !should_use_external_chunking(chunking_enabled, model_engine, diarization_enabled) {
        return transcribe_stream_collect(
            app_handle,
            client,
            base_url,
            options,
            abort_atomic,
            ProgressMode::Direct,
            None,
        )
        .await;
    }

    let media_duration = match probe_duration_seconds(audio_path) {
        Ok(duration) => duration,
        Err(error) => {
            tracing::warn!("unable to probe duration for Whisper chunking, using normal transcription: {error:?}");
            return transcribe_stream_collect(
                app_handle,
                client,
                base_url,
                options,
                abort_atomic,
                ProgressMode::Direct,
                None,
            )
            .await;
        }
    };

    if media_duration <= CHUNKING_MIN_DURATION_SECONDS {
        return transcribe_stream_collect(
            app_handle,
            client,
            base_url,
            options,
            abort_atomic,
            ProgressMode::Direct,
            None,
        )
        .await;
    }

    transcribe_chunked(
        app_handle,
        client,
        base_url,
        audio_path,
        options,
        abort_atomic,
        media_duration,
    )
    .await
}

async fn transcribe_chunked(
    app_handle: &tauri::AppHandle,
    client: &reqwest::Client,
    base_url: &str,
    audio_path: &Path,
    options: &TranscribeOptions,
    abort_atomic: &AtomicBool,
    media_duration: f64,
) -> Result<Vec<Segment>, CommandError> {
    if abort_atomic.load(Ordering::Relaxed) {
        return Ok(Vec::new());
    }

    let windows = plan_chunks(media_duration);
    tracing::info!(
        "Whisper fixed-window protection enabled: duration={:.2}s chunks={} request_ceiling={}s",
        media_duration,
        windows.len(),
        DEFAULT_CHUNK_SECONDS
    );

    let mut accepted_segments = Vec::new();
    let reported_progress = AtomicI64::new(0);

    for window in windows {
        if abort_atomic.load(Ordering::Relaxed) {
            tracing::debug!("chunked transcription aborted by user");
            break;
        }

        let chunk_path = extract_chunk(audio_path, window).map_err(CommandError::from)?;
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
        }

        let local_segments = transcription?;
        if abort_atomic.load(Ordering::Relaxed) {
            break;
        }

        let global_segments = globalize_segments(local_segments, window, media_duration);
        for segment in &global_segments {
            app_handle
                .emit_to("main", "new_segment", segment.clone())
                .log_error();
        }
        accepted_segments.extend(global_segments);
    }

    if !abort_atomic.load(Ordering::Relaxed) {
        let _ = set_progress_bar(app_handle, Some(100.0));
    }

    Ok(merge_segments(accepted_segments))
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
}
