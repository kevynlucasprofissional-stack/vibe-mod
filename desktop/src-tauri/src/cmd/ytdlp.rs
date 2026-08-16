use crate::diagnostics::DiagnosticsState;
use crate::error::LogError;
use crate::ffmpeg::get_vibe_temp_folder;
use eyre::{bail, Context, ContextCompat, Result};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tauri::{AppHandle, Emitter, Listener, Manager};

use super::files::get_ffmpeg_path;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::Stdio;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;
const STDERR_DIAGNOSTIC_BYTES: usize = 16 * 1024;

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
                diagnostics
                    .finish_run(
                        id,
                        if *cancelled { "aborted" } else { "succeeded" },
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

    result.map(|_| ())
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

    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_flag_c = cancel_flag.clone();
    app_handle.once("ytdlp-cancel", move |_| {
        cancel_flag_c.store(true, Ordering::Relaxed);
    });

    let mut child = cmd.spawn().context("failed to spawn yt-dlp")?;
    let stdout = child.stdout.take();
    let app_for_stdout = app_handle.clone();
    let stdout_thread = std::thread::spawn(move || {
        if let Some(stdout) = stdout {
            for line in BufReader::new(stdout).lines().map_while(|line| line.ok()) {
                let line = line.replace('\r', "").trim().to_string();
                if !line.starts_with("{\"progress") {
                    continue;
                }
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    let percentage = value["progress_str"]
                        .as_str()
                        .unwrap_or_default()
                        .trim()
                        .replace('%', "")
                        .parse::<f32>();
                    if let Ok(percentage) = percentage {
                        app_for_stdout.emit("ytdlp-progress", percentage).log_error();
                    }
                }
            }
        }
    });

    let recent_stderr = Arc::new(Mutex::new(String::new()));
    let stderr_capture = recent_stderr.clone();
    let stderr = child.stderr.take();
    let stderr_thread = std::thread::spawn(move || {
        if let Some(stderr) = stderr {
            for line in BufReader::new(stderr).lines().map_while(|line| line.ok()) {
                if let Ok(mut buffer) = stderr_capture.lock() {
                    buffer.push_str(&line);
                    buffer.push('\n');
                    trim_recent_utf8(&mut buffer, STDERR_DIAGNOSTIC_BYTES);
                }
            }
        }
    });

    let mut cancellation_logged = false;
    let status = loop {
        if cancel_flag.load(Ordering::Relaxed) && !cancellation_logged {
            cancellation_logged = true;
            diag(
                diagnostics,
                run_id,
                "warning",
                "ytdlp.cancel_requested",
                "yt-dlp cancellation requested; terminating child process",
                json!({}),
            );
            let _ = child.kill();
        }
        if let Some(status) = child.try_wait().context("failed to poll yt-dlp process")? {
            break status;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    let stderr_excerpt = recent_stderr
        .lock()
        .map(|buffer| buffer.trim().to_string())
        .unwrap_or_default();
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
            "recent_stderr": stderr_excerpt.clone(),
        }),
    );

    if !status.success() && !cancelled {
        bail!("Failed to download audio: {}", stderr_excerpt);
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

fn trim_recent_utf8(buffer: &mut String, max_bytes: usize) {
    if buffer.len() <= max_bytes {
        return;
    }
    let mut start = buffer.len().saturating_sub(max_bytes);
    while start < buffer.len() && !buffer.is_char_boundary(start) {
        start += 1;
    }
    buffer.drain(..start);
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
    use super::{safe_url_summary, trim_recent_utf8};

    #[test]
    fn diagnostic_url_summary_drops_sensitive_url_parts() {
        let sanitized = safe_url_summary("https://user:pass@example.org/watch?v=abc&token=secret#x");
        assert!(sanitized.contains("example.org/watch"));
        assert!(!sanitized.contains("secret"));
        assert!(!sanitized.contains("pass"));
        assert!(!sanitized.contains("token"));
    }

    #[test]
    fn recent_stderr_trim_preserves_utf8() {
        let mut value = format!("{}{}", "x".repeat(100), "á".repeat(20));
        trim_recent_utf8(&mut value, 31);
        assert!(value.len() <= 31);
        assert!(std::str::from_utf8(value.as_bytes()).is_ok());
    }
}
