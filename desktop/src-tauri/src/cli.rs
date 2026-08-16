use eyre::Result;
use serde_json::json;
use std::io::{BufRead, BufReader};
use std::process;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager};
use tauri_plugin_aptabase::EventTracker;

use crate::cmd::sona_cmd::{resolve_ffmpeg_path, resolve_sona_binary};
use crate::diagnostics::DiagnosticsState;
use crate::error::LogError;

const CLI_STDERR_DIAGNOSTIC_BYTES: usize = 16 * 1024;

#[cfg(all(windows, not(debug_assertions)))]
pub fn attach_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    let attach_result = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    if attach_result.is_ok() {
        unsafe {
            let conout = std::ffi::CString::new("CONOUT$").expect("CString::new failed");
            let stdout = libc_stdhandle::stdout();
            let stderr = libc_stdhandle::stderr();
            let mode = std::ffi::CString::new("w").unwrap();
            libc::freopen(conout.as_ptr(), mode.as_ptr(), stdout);
            libc::freopen(conout.as_ptr(), mode.as_ptr(), stderr);
        }
        tracing::debug!("CLI detected. attached console successfully");
    } else {
        tracing::debug!("No CLI detected.");
    }
}

pub fn is_cli_detected() -> bool {
    std::env::args().nth(1).is_some()
}

/// Forward all CLI args to the bundled Sona binary.
///
/// Vibe cannot observe individual decoder decisions inside this direct Sona
/// invocation. The diagnostic report therefore records process-level evidence:
/// command family, duration, exit status and recent stderr without persisting the
/// full argument vector, which may contain paths, prompts or credentials.
pub async fn run(app_handle: &AppHandle) -> Result<()> {
    #[cfg(target_os = "macos")]
    crate::dock::set_dock_visible(false);

    crate::analytics::track_event_handle(app_handle, crate::analytics::events::CLI_STARTED);

    let diagnostics = app_handle.state::<DiagnosticsState>();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command_family = args.first().cloned().unwrap_or_else(|| "unknown".to_string());
    let run_id = diagnostics
        .start_run(
            "cli",
            json!({
                "command_family": command_family,
                "argument_count": args.len(),
                "arguments_redacted": true,
                "visibility_note": "CLI forwards directly to Sona; Vibe records process-level evidence rather than per-request decoder internals."
            }),
        )
        .ok();
    let started_at = std::time::Instant::now();

    let result = run_sona_cli(app_handle, &args, &diagnostics, run_id.as_deref()).await;
    let exit_code = match result {
        Ok(code) => {
            if let Some(id) = run_id.as_deref() {
                diagnostics
                    .finish_run(
                        id,
                        if code == 0 { "succeeded" } else { "failed" },
                        json!({ "exit_code": code, "elapsed_ms": started_at.elapsed().as_millis() }),
                    )
                    .log_error();
            }
            code
        }
        Err(error) => {
            if let Some(id) = run_id.as_deref() {
                diagnostics
                    .record_event(
                        id,
                        "error",
                        "cli",
                        "cli.failed",
                        "CLI forwarding failed before a normal Sona exit",
                        json!({ "error": format!("{error:#}") }),
                    )
                    .log_error();
                diagnostics
                    .finish_run(
                        id,
                        "failed",
                        json!({ "error": format!("{error:#}"), "elapsed_ms": started_at.elapsed().as_millis() }),
                    )
                    .log_error();
            }
            1
        }
    };

    app_handle.flush_events_blocking();
    app_handle.cleanup_before_exit();
    process::exit(exit_code);
}

async fn run_sona_cli(
    app_handle: &AppHandle,
    args: &[String],
    diagnostics: &DiagnosticsState,
    run_id: Option<&str>,
) -> Result<i32> {
    let sona_binary = resolve_sona_binary(app_handle)?;
    let ffmpeg_path = resolve_ffmpeg_path(app_handle);
    if let Some(id) = run_id {
        diagnostics
            .record_event(
                id,
                "info",
                "cli",
                "cli.process_start",
                "Starting bundled Sona CLI process",
                json!({
                    "sona_binary_exists": sona_binary.exists(),
                    "ffmpeg_configured": ffmpeg_path.is_some(),
                }),
            )
            .log_error();
    }

    let mut cmd = std::process::Command::new(&sona_binary);
    cmd.args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(ref ffmpeg) = ffmpeg_path {
        cmd.env("SONA_FFMPEG_PATH", ffmpeg);
    }

    let mut child = cmd.spawn().map_err(|e| eyre::eyre!("failed to spawn sona: {}", e))?;
    let stdout = child.stdout.take();
    let stdout_thread = std::thread::spawn(move || {
        if let Some(out) = stdout {
            for line in BufReader::new(out).lines().map_while(|line| line.ok()) {
                println!("{}", line);
            }
        }
    });

    let recent_stderr = Arc::new(Mutex::new(String::new()));
    let stderr_capture = recent_stderr.clone();
    let stderr = child.stderr.take();
    let stderr_thread = std::thread::spawn(move || {
        if let Some(err) = stderr {
            for line in BufReader::new(err).lines().map_while(|line| line.ok()) {
                eprintln!("{}", line);
                if let Ok(mut buffer) = stderr_capture.lock() {
                    buffer.push_str(&line);
                    buffer.push('\n');
                    trim_recent_utf8(&mut buffer, CLI_STDERR_DIAGNOSTIC_BYTES);
                }
            }
        }
    });

    let status = child.wait().map_err(|e| eyre::eyre!("failed to wait for sona: {}", e))?;
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    let stderr = recent_stderr.lock().map(|value| value.trim().to_string()).unwrap_or_default();
    let exit_code = status.code().unwrap_or(1);

    if let Some(id) = run_id {
        diagnostics
            .record_event(
                id,
                if status.success() { "info" } else { "error" },
                "cli",
                "cli.process_exit",
                "Bundled Sona CLI process exited",
                json!({
                    "success": status.success(),
                    "exit_code": exit_code,
                    "recent_stderr": stderr,
                }),
            )
            .log_error();
    }
    Ok(exit_code)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_stderr_buffer_keeps_recent_utf8() {
        let mut value = format!("{}{}", "x".repeat(100), "á".repeat(20));
        trim_recent_utf8(&mut value, 31);
        assert!(value.len() <= 31);
        assert!(std::str::from_utf8(value.as_bytes()).is_ok());
    }
}
