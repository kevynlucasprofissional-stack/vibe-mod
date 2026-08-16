use crate::error::LogError;
use crate::ffmpeg::get_vibe_temp_folder;
use crate::{cmd::app::get_logs_folder, config, logging::get_log_path};
use eyre::{eyre, ContextCompat, Result};
use std::path::Path;
use std::time::{Duration, SystemTime};
use tauri::Manager;

const RAW_LOG_RETENTION_DAYS: u64 = 14;
const DIAGNOSTIC_RETENTION_DAYS: u64 = 30;

pub fn clean_old_logs(app: &tauri::AppHandle) -> Result<()> {
    tracing::debug!("clean old logs older than {} days", RAW_LOG_RETENTION_DAYS);
    let current_log_path = get_log_path(&app.clone())?;
    let logs_folder = get_logs_folder(app.to_owned())?;
    let logs_folder = logs_folder.to_str().context("tostr")?;
    let logs_folder = logs_folder.strip_suffix('/').unwrap_or(logs_folder);
    let logs_folder = logs_folder.strip_suffix('\\').unwrap_or(logs_folder);
    let pattern = format!(
        "{}/{}*{}",
        logs_folder,
        config::LOG_FILENAME_PREFIX,
        config::LOG_FILENAME_SUFFIX
    );

    for path in glob::glob(&pattern)? {
        let path = path?;
        if path == current_log_path || !is_older_than(&path, RAW_LOG_RETENTION_DAYS) {
            continue;
        }
        tracing::debug!("clean expired raw log {}", path.display());
        std::fs::remove_file(path)?;
    }
    Ok(())
}

pub fn clean_old_diagnostics(app: &tauri::AppHandle) -> Result<()> {
    let root = app.path().app_config_dir()?.join("diagnostics");
    if !root.exists() {
        return Ok(());
    }
    tracing::debug!(
        "clean diagnostic reports older than {} days in {}",
        DIAGNOSTIC_RETENTION_DAYS,
        root.display()
    );
    for day in std::fs::read_dir(&root)? {
        let day = day?;
        let day_path = day.path();
        if !day_path.is_dir() {
            if is_older_than(&day_path, DIAGNOSTIC_RETENTION_DAYS) {
                std::fs::remove_file(day_path).log_error();
            }
            continue;
        }
        for report in std::fs::read_dir(&day_path)? {
            let report_path = report?.path();
            if report_path.is_file() && is_older_than(&report_path, DIAGNOSTIC_RETENTION_DAYS) {
                tracing::debug!("clean expired diagnostic report {}", report_path.display());
                std::fs::remove_file(report_path).log_error();
            }
        }
        if std::fs::read_dir(&day_path)?.next().is_none() {
            std::fs::remove_dir(day_path).log_error();
        }
    }
    Ok(())
}

fn is_older_than(path: &Path, days: u64) -> bool {
    let Ok(modified) = path.metadata().and_then(|metadata| metadata.modified()) else {
        return false;
    };
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(days.saturating_mul(24 * 60 * 60)))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    modified < cutoff
}

pub fn clean_old_files() -> Result<()> {
    let current_temp_dir = get_vibe_temp_folder();
    let temp_dir = std::env::temp_dir();
    let temp_dir = temp_dir.to_str().unwrap_or_default();
    let temp_dir = temp_dir.strip_suffix('/').unwrap_or(temp_dir);
    let temp_dir = temp_dir.strip_suffix('\\').unwrap_or(temp_dir);
    let pattern = format!("{}/vibe_temp*", temp_dir);
    tracing::debug!("searching old files in {}", pattern);
    for path in glob::glob(&pattern)? {
        let path = path?;
        if path == current_temp_dir {
            tracing::debug!("Skip deletion of {}", current_temp_dir.display());
            continue;
        }
        tracing::debug!("Clean old folder {}", path.clone().display());
        std::fs::remove_dir_all(path.clone())
            .map_err(|e| eyre!("failed to delete {}: {:?}", path.display(), e))
            .log_error();
    }
    Ok(())
}

pub fn clean_updater_files() -> Result<()> {
    let current_temp_dir = get_vibe_temp_folder();
    let temp_dir = std::env::temp_dir();
    let temp_dir = temp_dir.to_str().unwrap_or_default();
    let temp_dir = temp_dir.strip_suffix('/').unwrap_or(temp_dir);
    let temp_dir = temp_dir.strip_suffix('\\').unwrap_or(temp_dir);
    let pattern = format!("{}/vibe*-updater*", temp_dir);
    tracing::debug!("searching old files in {}", pattern);
    for path in glob::glob(&pattern)? {
        let path = path?;
        if path == current_temp_dir {
            tracing::debug!("Skip deletion of {}", current_temp_dir.display());
            continue;
        }
        tracing::debug!("Clean old folder {}", path.display());
        if path.is_dir() {
            std::fs::remove_dir_all(&path)
                .map_err(|e| eyre!("failed to delete {}: {:?}", path.display(), e))
                .log_error();
        } else {
            tracing::debug!("Skipping non-directory path {}", path.display());
        }
    }
    Ok(())
}
