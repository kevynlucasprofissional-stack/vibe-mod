use crate::diagnostics::DiagnosticsState;
use crate::ffmpeg::get_vibe_temp_folder;
use eyre::{bail, Context, ContextCompat, Result};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};
use tauri::{AppHandle, Emitter, Listener, Manager};

use crate::error::LogError;
use super::files::get_ffmpeg_path;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::Stdio;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

fn get_binary_name() -> &'static str {
    if cfg!(windows) {
        if cfg!(target_arch = "aarch64") {
            "yt-dlp_arm64.exe"
        } else {
            "yt-dlp.exe"
        }
    } else if cfg!(target_os = "linux") {
        if cfg!(target_arch = "aarch64") {
            "yt-dlp_linux_aarch64"
        } else {
            "yt-dlp_linux"
        }
    } else {
        "yt-dlp_macos"
    }
}

#[tauri::command]
pub async fn get_latest_ytdlp_version() -> Result<String> {
    let client = reqwest::Client::builder().user_agent("vibe-app").build()?;
    let resp = client
        .get("https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest")
        .send()
        .await?
        .error_for_status()?;
    let json: Value = resp.json().await?;
    json["tag_name"]
        .as_str()
        .context("missing tag_name in latest release response")
        .map(ToString::to_string)
}

#[tauri::command]
pub fn get_temp_path(app_handle: AppHandle, ext: String, in_documents: Option<bool>, custom_path: Option<String>) -> String {
    let mut base_path = if in_documents.unwrap_or_default() {
        let dir = if let Some(ref cp) = custom_path {
            PathBuf::from(cp)
        } else {
            app_handle
                .path()
                .document_dir()
                .unwrap_or(get_vibe_temp_folder())
                .join(crate::config::DOCUMENTS_SUBFOLDER)
        };
        std::fs::create_dir_all(&dir).ok();
        dir
    } else {
        get_vibe_temp_folder()
    };

    base_path.push(format!("{}.{}", crate::ffmpeg::get_local_time(), ext));
    base_path.to_string_lossy().to_string()
}

#[tauri::command]
pub async fn download_audio(app_handle: AppHandle, url: String, out_path: String) -> Result<()> {
    tracing::debug!("download audio from {}", safe_url_summary(&url));
    let diagnostics = app_handle.state::<DiagnosticsState>();
    let binary_name = get_binary_name();
    let binary_dir = app_handle.path().app_local_data_dir().context("Can't get data directory")?;
    let binary_path = binary_dir.join(binary_name);
    let ffmpeg_path = get_ffmpeg_path();
    let run_id = diagnostics
        .start_run(
            "ytdlp_download",
            json!({
                "url": safe_url_summary(&url),
                "output_path": out_path.clone(),
                "binary": binary_path.clone(),
                "binary_exists": binary_path.exists(),
                "ffmpeg_path": ffmpeg_path.clone(),
            }),
        )
        .ok();

    let result = download_audio_inner(
        &app_handle,
        &url,
        &out_path,
        binary_path,
        &ffmpeg_path,
        &diagnostics,
        run_id.as_deref(),
    )
    .await;

    if let Some(id) = run_id.as_deref() {
        match &result {
            Ok(cancelled) => {
                let outcome = if *cancelled { "aborted" } else { "succeeded" };
                diagnostics
                    .finish_run(
                        id,
                        outcome,
                        json!({
                            "cancelled": cancelled,
                            "output_path": out_path.clone(),
                            "output_exists": std::path::Path::new(&out_path).exists(),
                            "output_size_bytes": std::fs::metadata(&out_path).ok().map(|metadata| metadata.len()),
                        }),
                    )
                    .log_error();
            }
            Err(error) => {
                diagnostics
                    .finish_run(id, "failed", json!({ "error": format!("{error:#}") }))
                    .log_error();
            }
        }
    }

    match result {
        Ok(_) => Ok(()),
        Err(error) => Err(error),
    }
}

async fn download_audio_inner(
    app_handle: &AppHandle,
    url: &str,
    out_path: &str,
    binary_path: PathBuf,
    ffmpeg_path: &str,
    diagnostics: &DiagnosticsState,
    run_id: Option<&str>,
) -> Result<bool> {
    if !binary_path.exists() {
        diag(
            diagnostics,
            run_id,
            "error",
            "ytdlp.binary_missing",
            "yt-dlp binary is missing",
            json!({ "binary": binary_path.clone() }),
        );
        bail!("yt-dlp binary not found at {}", binary_path.display());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(binary_path.clone())?;
        let mut perm = meta.permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(binary_path.clone(), perm)?;
    }

    diag(
        diagnostics,
        run_id,
        "info",
        "ytdlp.process_start",
        "Starting yt-dlp audio extraction",
        json!({
            "binary": binary_path.clone(),
            "ffmpeg_path": ffmpeg_path,
            "audio_format": "m4a",
            "playlist": false,
        }),
    );

    let mut cmd = std::process::Command::new(binary_path);
    let cmd = cmd
        .args([
            "--progress-template",
            "{\"progress\": \"%(progress.percent)s\", \"total_bytes\": \"%(progress.total_bytes)s\", \"progress_str\": \"%(progress._percent_str)s\"}\n",
            "--no-playlist",
            "-x",
            "--audio-format",
            "m4a",
            "--ffmpeg-location",
            ffmpeg_path,
            url,
            "-o",
            out_path,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    let cmd = cmd.creation_flags(CREATE_NO_WINDOW);

    let cancel_flag = std::sync::Arc::new(AtomicBool::new(false));
    let cancel_flag_c = cancel_flag.clone();
    app_handle.once("ytdlp-cancel", move |_| {
        cancel_flag_c.store(true, Ordering::Relaxed);
    });

    let mut child = cmd.spawn().context("failed to spawn yt-dlp")?;
    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            if cancel_flag.load(Ordering::Relaxed) {
                diag(
                    diagnostics,
                    run_id,
                    "warning",
                    "ytdlp.cancel_requested",
                    "yt-dlp cancellation requested; terminating child process",
                    json!({}),
                );
                let _ = child.kill();
                break;
            }

            let line = line?.replace('\r', "").trim().to_string();
            if line.starts_with("{\"progress") {
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    let percentage_str = value["progress_str"]
                        .as_str()
                        .unwrap_or_default()
                        .trim()
                        .replace('%', "");
                    if let Ok(percentage_number) = percentage_str.parse::<f32>() {
                        app_handle
                            .emit("ytdlp-progress", percentage_number)
                            .context("failed to emit")
                            .log_error();
                    }
                }
            }
        }
    }

    let status = child.wait()?;
    let stderr_output = child
        .stderr
        .take()
        .map(|stderr| {
            BufReader::new(stderr)
                .lines()
                .map_while(Result::ok)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let stderr_excerpt: String = stderr_output.chars().rev().take(16_384).collect::<String>().chars().rev().collect();
    let cancelled = cancel_flag.load(Ordering::Relaxed);

    diag(
        diagnostics,
        run_id,
        if status.success() { "info" } else if cancelled { "warning" } else { "error" },
        "ytdlp.process_exit",
        "yt-dlp child process exited",
        json!({
            "success": status.success(),
            "exit_code": status.code(),
            "cancelled": cancelled,
            "stderr": stderr_excerpt,
        }),
    );

    if !status.success() && !cancelled {
        bail!("Failed to download audio: {}", stderr_output);
    }
    Ok(cancelled)
}

fn diag(
    diagnostics: &DiagnosticsState,
    run_id: Option<&str>,
    severity: &str,
    stage: &str,
    message: &str,
    data: Value,
) {
    if let Some(id) = run_id {
        diagnostics
            .record_event(id, severity, "ytdlp", stage, message, data)
            .log_error();
    }
}

fn safe_url_summary(value: &str) -> String {
    match url::Url::parse(value) {
        Ok(mut parsed) => {
            parsed.set_query(None);
            parsed.set_fragment(None);
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
            parsed.to_string()
        }
        Err(_) => "<invalid-or-non-url>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::safe_url_summary;

    #[test]
    fn diagnostic_url_summary_drops_sensitive_url_parts() {
        let sanitized = safe_url_summary("https://user:pass@example.org/watch?v=abc&token=secret#x");
        assert!(sanitized.contains("example.org/watch"));
        assert!(!sanitized.contains("secret"));
        assert!(!sanitized.contains("pass"));
        assert!(!sanitized.contains("token"));
    }
}
