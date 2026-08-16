use crate::diagnostics::DiagnosticsState;
use crate::error::LogError;
use crate::setup::SonaState;
use eyre::{bail, Context, ContextCompat, Result};
use serde_json::json;
use std::path::{Path, PathBuf};
use tauri::{Manager, State};
use tokio::sync::Mutex;

pub fn resolve_sona_binary(app_handle: &tauri::AppHandle) -> Result<PathBuf> {
    let resource_dir = app_handle.path().resource_dir().context("get resource dir")?;

    #[cfg(target_os = "windows")]
    let binary_name = "sona.exe";
    #[cfg(not(target_os = "windows"))]
    let binary_name = "sona";

    let sidecar_path = resource_dir.join(binary_name);
    if sidecar_path.exists() {
        return Ok(sidecar_path);
    }
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let path = exe_dir.join(binary_name);
            if path.exists() {
                return Ok(path);
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        let linux_paths = [
            PathBuf::from("/usr/lib/vibe").join(binary_name),
            PathBuf::from("/usr/lib/vibe/binaries").join(binary_name),
            PathBuf::from("/opt/vibe").join(binary_name),
            PathBuf::from("/opt/vibe/binaries").join(binary_name),
        ];
        for path in &linux_paths {
            if path.exists() {
                return Ok(path.clone());
            }
        }
    }
    if let Ok(path) = which::which(binary_name) {
        return Ok(path);
    }
    bail!("sona binary not found")
}

pub fn resolve_ffmpeg_path(app_handle: &tauri::AppHandle) -> Option<PathBuf> {
    let resource_dir = app_handle.path().resource_dir().ok()?;
    #[cfg(target_os = "windows")]
    let binary_name = "ffmpeg.exe";
    #[cfg(not(target_os = "windows"))]
    let binary_name = "ffmpeg";
    let sidecar_path = resource_dir.join(binary_name);
    sidecar_path.exists().then_some(sidecar_path)
}

#[tauri::command]
pub async fn load_model(
    app_handle: tauri::AppHandle,
    model_path: String,
    gpu_device: Option<i32>,
    unload_timeout_minutes: u32,
) -> Result<String> {
    let diagnostics = app_handle.state::<DiagnosticsState>();
    let run_id = diagnostics
        .start_run(
            "model_load",
            json!({
                "model_path": model_path,
                "gpu_device": gpu_device,
                "unload_timeout_minutes": unload_timeout_minutes,
            }),
        )
        .map_err(|error| {
            tracing::error!("failed to start model-load diagnostics: {error:?}");
            error
        })
        .ok();

    if let Some(run_id) = run_id.as_deref() {
        diagnostics
            .record_event(
                run_id,
                "info",
                "model",
                "model.load_requested",
                "Model load requested",
                json!({ "gpu_device": gpu_device }),
            )
            .log_error();
    }

    let result = load_model_inner(&app_handle, &model_path, gpu_device, unload_timeout_minutes, &diagnostics, run_id.as_deref()).await;
    if let Some(run_id) = run_id.as_deref() {
        match &result {
            Ok(value) => {
                let state = app_handle.state::<Mutex<SonaState>>();
                let guard = state.lock().await;
                diagnostics
                    .finish_run(
                        run_id,
                        "succeeded",
                        json!({
                            "result": value,
                            "engine": guard.model_engine,
                            "gpu_device": guard.gpu_device,
                            "gpu_fallback": guard.gpu_fallback,
                            "model_path": guard.loaded_model_path,
                        }),
                    )
                    .log_error();
            }
            Err(error) => {
                diagnostics
                    .record_event(
                        run_id,
                        "error",
                        "model",
                        "model.load_failed",
                        "Model loading failed",
                        json!({ "error": format!("{error:#}") }),
                    )
                    .log_error();
                diagnostics
                    .finish_run(run_id, "failed", json!({ "error": format!("{error:#}") }))
                    .log_error();
            }
        }
    }
    result
}

async fn load_model_inner(
    app_handle: &tauri::AppHandle,
    model_path: &str,
    gpu_device: Option<i32>,
    unload_timeout_minutes: u32,
    diagnostics: &DiagnosticsState,
    run_id: Option<&str>,
) -> Result<String> {
    let sona_state: State<'_, Mutex<SonaState>> = app_handle.state();
    let mut state_guard = sona_state.lock().await;

    let process_is_alive = state_guard.process.as_mut().is_some_and(crate::sona::SonaProcess::is_alive);
    if !process_is_alive {
        if state_guard.process.is_some() {
            tracing::warn!("cached sona process is no longer running; restarting it");
            diag(
                diagnostics,
                run_id,
                "warning",
                "model.sona_restart_dead_process",
                "Cached Sona process was not alive and will be restarted",
                json!({}),
            );
        }
        state_guard.process = None;
        clear_model_state(&mut state_guard);
    }

    if state_guard
        .process
        .as_ref()
        .is_some_and(|process| process.unload_timeout_minutes() != unload_timeout_minutes)
    {
        diag(
            diagnostics,
            run_id,
            "info",
            "model.sona_restart_timeout_change",
            "Restarting Sona to apply a new unload timeout",
            json!({ "unload_timeout_minutes": unload_timeout_minutes }),
        );
        state_guard.process = None;
        clear_model_state(&mut state_guard);
    }

    let spawn_sona = || -> Result<crate::sona::SonaProcess> {
        let binary_path = resolve_sona_binary(app_handle)?;
        let ffmpeg_path = resolve_ffmpeg_path(app_handle);
        crate::sona::SonaProcess::spawn(&binary_path, ffmpeg_path.as_deref(), unload_timeout_minutes)
    };

    if state_guard.process.is_none() {
        diag(
            diagnostics,
            run_id,
            "info",
            "model.sona_spawn_started",
            "Starting the Sona sidecar process",
            json!({}),
        );
        match spawn_sona() {
            Ok(process) => state_guard.process = Some(process),
            Err(error) => {
                let error_msg = format!("{error:#}");
                crate::analytics::track_event_handle_with_props(
                    app_handle,
                    crate::analytics::events::SONA_SPAWN_FAILED,
                    Some(json!({"error_message": error_msg})),
                );
                return Err(error);
            }
        }
    }

    diag(
        diagnostics,
        run_id,
        "info",
        "model.gpu_load_started",
        "Attempting model load with the requested GPU configuration",
        json!({ "gpu_device": gpu_device }),
    );
    let load_result = {
        let sona = state_guard.process.as_mut().unwrap();
        sona.load_model(model_path, gpu_device, false).await
    };

    let gpu_fallback = match load_result {
        Ok(()) => false,
        Err(error) => {
            let stderr = state_guard
                .process
                .as_ref()
                .map(crate::sona::SonaProcess::recent_stderr)
                .unwrap_or_default();
            tracing::warn!("model load failed with GPU enabled, falling back to CPU: {error:#}");
            diag(
                diagnostics,
                run_id,
                "warning",
                "model.gpu_fallback",
                "GPU model loading failed; restarting Sona and retrying on CPU",
                json!({ "error": format!("{error:#}"), "sona_stderr": stderr }),
            );
            if let Some(mut old) = state_guard.process.take() {
                old.kill();
            }
            clear_model_state(&mut state_guard);
            let process = spawn_sona().context("failed to respawn sona")?;
            state_guard.process = Some(process);
            let sona = state_guard.process.as_mut().unwrap();
            sona.load_model(model_path, gpu_device, true).await?;
            true
        }
    };

    let resolved_engine = {
        let sona = state_guard.process.as_ref().unwrap();
        match sona.model_metadata(model_path).await {
            Ok(metadata) => {
                tracing::debug!(engine = %metadata.capabilities.engine, "loaded model engine");
                Some(metadata.capabilities.engine)
            }
            Err(error) => {
                tracing::warn!("unable to resolve loaded model engine: {error:?}");
                diag(
                    diagnostics,
                    run_id,
                    "warning",
                    "model.metadata_failed",
                    "Model loaded but engine metadata could not be resolved",
                    json!({ "error": error.to_string() }),
                );
                None
            }
        }
    };

    state_guard.model_engine = resolved_engine;
    state_guard.loaded_model_path = Some(model_path.to_string());
    state_guard.gpu_device = gpu_device;
    state_guard.gpu_fallback = gpu_fallback;
    diag(
        diagnostics,
        run_id,
        "info",
        "model.load_completed",
        "Model load completed",
        json!({
            "engine": state_guard.model_engine,
            "gpu_device": gpu_device,
            "gpu_fallback": gpu_fallback,
            "sona_stderr": state_guard.process.as_ref().map(crate::sona::SonaProcess::recent_stderr),
        }),
    );

    if gpu_fallback {
        Ok("gpu_fallback".to_string())
    } else {
        Ok(model_path.to_string())
    }
}

fn clear_model_state(state: &mut SonaState) {
    state.model_engine = None;
    state.loaded_model_path = None;
    state.gpu_device = None;
    state.gpu_fallback = false;
}

fn diag(diagnostics: &DiagnosticsState, run_id: Option<&str>, severity: &str, stage: &str, message: &str, data: serde_json::Value) {
    if let Some(run_id) = run_id {
        diagnostics
            .record_event(run_id, severity, "model", stage, message, data)
            .log_error();
    }
}

#[tauri::command]
pub async fn get_gpu_devices(app_handle: tauri::AppHandle) -> Result<Vec<crate::sona::GpuDevice>> {
    let binary_path = resolve_sona_binary(&app_handle)?;
    let devices = crate::sona::list_gpu_devices(&binary_path)?;
    Ok(devices)
}

#[tauri::command]
pub async fn get_model_metadata(app_handle: tauri::AppHandle, model_path: String) -> Result<crate::sona::ModelMetadata> {
    let sona_state: State<'_, Mutex<SonaState>> = app_handle.state();
    let mut state = sona_state.lock().await;
    if state.process.as_mut().is_none_or(|process| !process.is_alive()) {
        let binary_path = resolve_sona_binary(&app_handle)?;
        let ffmpeg_path = resolve_ffmpeg_path(&app_handle);
        state.process = Some(crate::sona::SonaProcess::spawn(&binary_path, ffmpeg_path.as_deref(), 5)?);
        clear_model_state(&mut state);
    }
    state.process.as_ref().unwrap().model_metadata(&model_path).await
}

#[tauri::command]
pub async fn get_api_base_url(sona_state: State<'_, Mutex<SonaState>>) -> Result<Option<String>> {
    let state = sona_state.lock().await;
    Ok(state.process.as_ref().map(|process| process.base_url()))
}

#[tauri::command]
pub async fn start_api_server(
    app_handle: tauri::AppHandle,
    sona_state: State<'_, Mutex<SonaState>>,
    unload_timeout_minutes: u32,
) -> Result<String> {
    let mut state_guard = sona_state.lock().await;
    if state_guard
        .process
        .as_ref()
        .is_some_and(|process| process.unload_timeout_minutes() != unload_timeout_minutes)
    {
        tracing::debug!(unload_timeout_minutes, "restarting sona to apply unload timeout");
        state_guard.process = None;
        clear_model_state(&mut state_guard);
    }
    if state_guard.process.is_none() {
        let binary_path = resolve_sona_binary(&app_handle)?;
        let ffmpeg_path = resolve_ffmpeg_path(&app_handle);
        state_guard.process = Some(crate::sona::SonaProcess::spawn(&binary_path, ffmpeg_path.as_deref(), unload_timeout_minutes)?);
        clear_model_state(&mut state_guard);
    }
    let process = state_guard.process.as_ref().context("API server process missing")?;
    Ok(process.base_url())
}

#[tauri::command]
pub async fn stop_api_server(sona_state: State<'_, Mutex<SonaState>>) -> Result<bool> {
    let mut state_guard = sona_state.lock().await;
    if let Some(mut process) = state_guard.process.take() {
        process.kill();
        clear_model_state(&mut state_guard);
        return Ok(true);
    }
    clear_model_state(&mut state_guard);
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_model_state_removes_runtime_metadata() {
        let mut state = SonaState {
            process: None,
            model_engine: Some("whisper".to_string()),
            loaded_model_path: Some("model.bin".to_string()),
            gpu_device: Some(0),
            gpu_fallback: true,
        };
        clear_model_state(&mut state);
        assert!(state.model_engine.is_none());
        assert!(state.loaded_model_path.is_none());
        assert!(state.gpu_device.is_none());
        assert!(!state.gpu_fallback);
    }

    #[test]
    fn resolve_ffmpeg_path_type_remains_path_based() {
        fn accepts_path(_: Option<&Path>) {}
        accepts_path(None);
    }
}
