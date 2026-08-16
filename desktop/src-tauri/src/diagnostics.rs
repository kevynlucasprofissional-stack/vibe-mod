use crate::cmd::app::{get_commit_hash, is_avx2_enabled};
use crate::ffmpeg::random_string;
use crate::transcript::Segment;
use chrono::{Local, Utc};
use eyre::{Context, Result};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::{Manager, State};

const DIAGNOSTIC_SCHEMA_VERSION: &str = "vibe-diagnostics/1";
const MAX_EVENTS_PER_RUN: usize = 10_000;

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticReportPaths {
    pub json: String,
    pub markdown: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticEvent {
    pub timestamp: String,
    pub severity: String,
    pub category: String,
    pub stage: String,
    pub message: String,
    pub data: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticItem {
    pub item_id: String,
    pub label: String,
    pub status: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub data: Value,
    pub error: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticAnomaly {
    pub code: String,
    pub severity: String,
    pub message: String,
    pub evidence: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticRun {
    pub schema_version: String,
    pub run_id: String,
    pub source: String,
    pub status: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub duration_ms: Option<u128>,
    pub app: Value,
    pub context: Value,
    pub events: Vec<DiagnosticEvent>,
    pub items: Vec<DiagnosticItem>,
    pub anomalies: Vec<DiagnosticAnomaly>,
    pub result: Value,
    pub evidence: Value,
    pub privacy: Value,
    pub report_files: DiagnosticReportPaths,
    #[serde(skip_serializing)]
    started_instant: std::time::Instant,
    #[serde(skip_serializing)]
    report_dir: PathBuf,
}

struct DiagnosticsStore {
    active: HashMap<String, DiagnosticRun>,
    latest_report: Option<PathBuf>,
}

pub struct DiagnosticsState {
    root: PathBuf,
    app_snapshot: Value,
    raw_log_path: Option<PathBuf>,
    store: Mutex<DiagnosticsStore>,
}

impl DiagnosticsState {
    pub fn new(app: &tauri::AppHandle) -> Result<Self> {
        let root = app.path().app_config_dir()?.join("diagnostics");
        fs::create_dir_all(&root).context("failed to create diagnostics directory")?;
        let raw_log_path = crate::logging::get_log_path(app).ok();
        let latest_report = newest_report(&root);
        Ok(Self {
            root,
            app_snapshot: app_snapshot(app),
            raw_log_path,
            store: Mutex::new(DiagnosticsStore {
                active: HashMap::new(),
                latest_report,
            }),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn start_run(&self, source: impl Into<String>, context: Value) -> Result<String> {
        let now = Utc::now();
        let run_id = format!("{}-{}", now.format("%Y%m%dT%H%M%S%3fZ"), random_string(8));
        let report_dir = self.root.join(Local::now().format("%Y-%m-%d").to_string());
        fs::create_dir_all(&report_dir).context("failed to create run diagnostics directory")?;
        let json_path = report_dir.join(format!("{run_id}.json"));
        let md_path = report_dir.join(format!("{run_id}.md"));
        let report_files = DiagnosticReportPaths {
            json: sanitize_path_string(&json_path),
            markdown: sanitize_path_string(&md_path),
        };
        let mut run = DiagnosticRun {
            schema_version: DIAGNOSTIC_SCHEMA_VERSION.to_string(),
            run_id: run_id.clone(),
            source: source.into(),
            status: "running".to_string(),
            started_at: now.to_rfc3339(),
            finished_at: None,
            duration_ms: None,
            app: self.app_snapshot.clone(),
            context: sanitize_value(context),
            events: Vec::new(),
            items: Vec::new(),
            anomalies: Vec::new(),
            result: json!({}),
            evidence: json!({
                "raw_log_path": self.raw_log_path.as_ref().map(|path| sanitize_path_string(path)),
                "note": "The raw tracing log is complementary evidence; the structured report is the primary reconstruction artifact."
            }),
            privacy: json!({
                "secrets": "Keys whose names look like passwords, API keys, tokens, authorization headers or secrets are replaced with <redacted>.",
                "paths": "The current user's home directory is replaced with $HOME where possible.",
                "audio": "Audio bytes are never copied into the diagnostic report.",
                "accuracy": "Without a human reference transcript, quality findings are anomaly signals, not measured transcription accuracy."
            }),
            report_files,
            started_instant: std::time::Instant::now(),
            report_dir,
        };
        run.events.push(DiagnosticEvent {
            timestamp: Utc::now().to_rfc3339(),
            severity: "info".to_string(),
            category: "lifecycle".to_string(),
            stage: "run.start".to_string(),
            message: "Diagnostic run started".to_string(),
            data: json!({}),
        });
        persist_run(&run, false)?;
        self.store.lock().expect("diagnostics lock").active.insert(run_id.clone(), run);
        Ok(run_id)
    }

    pub fn record_event(
        &self,
        run_id: &str,
        severity: &str,
        category: &str,
        stage: &str,
        message: &str,
        data: Value,
    ) -> Result<()> {
        let mut store = self.store.lock().expect("diagnostics lock");
        let Some(run) = store.active.get_mut(run_id) else {
            return Ok(());
        };
        if run.events.len() >= MAX_EVENTS_PER_RUN {
            if !run.anomalies.iter().any(|item| item.code == "event_limit_reached") {
                run.anomalies.push(DiagnosticAnomaly {
                    code: "event_limit_reached".to_string(),
                    severity: "warning".to_string(),
                    message: format!("Diagnostic event limit ({MAX_EVENTS_PER_RUN}) reached; subsequent events are not recorded."),
                    evidence: json!({}),
                });
            }
            return Ok(());
        }
        run.events.push(DiagnosticEvent {
            timestamp: Utc::now().to_rfc3339(),
            severity: severity.to_string(),
            category: category.to_string(),
            stage: stage.to_string(),
            message: message.to_string(),
            data: sanitize_value(data),
        });
        persist_run(run, false)
    }

    pub fn upsert_item(
        &self,
        run_id: &str,
        item_id: &str,
        label: &str,
        status: &str,
        data: Value,
        error: Option<Value>,
    ) -> Result<()> {
        let mut store = self.store.lock().expect("diagnostics lock");
        let Some(run) = store.active.get_mut(run_id) else {
            return Ok(());
        };
        let now = Utc::now().to_rfc3339();
        if let Some(item) = run.items.iter_mut().find(|item| item.item_id == item_id) {
            item.status = status.to_string();
            item.data = sanitize_value(data);
            item.error = error.map(sanitize_value);
            if item.started_at.is_none() && status == "running" {
                item.started_at = Some(now.clone());
            }
            if is_terminal_status(status) {
                item.finished_at = Some(now);
            }
        } else {
            run.items.push(DiagnosticItem {
                item_id: item_id.to_string(),
                label: label.to_string(),
                status: status.to_string(),
                started_at: (status == "running").then_some(now.clone()),
                finished_at: is_terminal_status(status).then_some(now),
                data: sanitize_value(data),
                error: error.map(sanitize_value),
            });
        }
        persist_run(run, false)
    }

    pub fn add_anomaly(&self, run_id: &str, anomaly: DiagnosticAnomaly) -> Result<()> {
        let mut store = self.store.lock().expect("diagnostics lock");
        let Some(run) = store.active.get_mut(run_id) else {
            return Ok(());
        };
        if !run.anomalies.iter().any(|existing| existing.code == anomaly.code && existing.evidence == anomaly.evidence) {
            run.anomalies.push(DiagnosticAnomaly {
                evidence: sanitize_value(anomaly.evidence),
                ..anomaly
            });
        }
        persist_run(run, false)
    }

    pub fn finish_run(&self, run_id: &str, outcome: &str, result: Value) -> Result<Option<DiagnosticReportPaths>> {
        let mut store = self.store.lock().expect("diagnostics lock");
        let Some(mut run) = store.active.remove(run_id) else {
            return Ok(None);
        };
        run.status = outcome.to_string();
        run.finished_at = Some(Utc::now().to_rfc3339());
        run.duration_ms = Some(run.started_instant.elapsed().as_millis());
        run.result = sanitize_value(result);
        run.events.push(DiagnosticEvent {
            timestamp: Utc::now().to_rfc3339(),
            severity: if outcome == "succeeded" { "info" } else { "warning" }.to_string(),
            category: "lifecycle".to_string(),
            stage: "run.finish".to_string(),
            message: format!("Diagnostic run finished with outcome: {outcome}"),
            data: json!({}),
        });
        persist_run(&run, true)?;
        let json_path = run.report_dir.join(format!("{}.json", run.run_id));
        store.latest_report = Some(json_path);
        Ok(Some(run.report_files.clone()))
    }

    pub fn latest_report_path(&self) -> Option<PathBuf> {
        self.store.lock().expect("diagnostics lock").latest_report.clone().or_else(|| newest_report(&self.root))
    }

    pub fn record_crash_best_effort(&self, message: &str) {
        let Ok(mut store) = self.store.try_lock() else {
            return;
        };
        for run in store.active.values_mut() {
            run.status = "crashed".to_string();
            run.finished_at = Some(Utc::now().to_rfc3339());
            run.duration_ms = Some(run.started_instant.elapsed().as_millis());
            run.events.push(DiagnosticEvent {
                timestamp: Utc::now().to_rfc3339(),
                severity: "error".to_string(),
                category: "crash".to_string(),
                stage: "process.crash".to_string(),
                message: message.to_string(),
                data: json!({}),
            });
            let _ = persist_run(run, true);
        }
    }

    pub fn finalize_active_best_effort(&self, outcome: &str) {
        let Ok(mut store) = self.store.try_lock() else {
            return;
        };
        let ids: Vec<String> = store.active.keys().cloned().collect();
        for id in ids {
            if let Some(mut run) = store.active.remove(&id) {
                run.status = outcome.to_string();
                run.finished_at = Some(Utc::now().to_rfc3339());
                run.duration_ms = Some(run.started_instant.elapsed().as_millis());
                run.events.push(DiagnosticEvent {
                    timestamp: Utc::now().to_rfc3339(),
                    severity: "warning".to_string(),
                    category: "lifecycle".to_string(),
                    stage: "run.interrupted".to_string(),
                    message: format!("Run finalized because the application ended: {outcome}"),
                    data: json!({}),
                });
                if persist_run(&run, true).is_ok() {
                    store.latest_report = Some(run.report_dir.join(format!("{}.json", run.run_id)));
                }
            }
        }
    }
}

#[tauri::command]
pub fn diagnostics_start_run(
    state: State<'_, DiagnosticsState>,
    source: String,
    context: Option<Value>,
) -> std::result::Result<String, String> {
    state.start_run(source, context.unwrap_or_else(|| json!({}))).map_err(|error| error.to_string())
}

#[tauri::command]
pub fn diagnostics_record_event(
    state: State<'_, DiagnosticsState>,
    run_id: String,
    severity: String,
    category: String,
    stage: String,
    message: String,
    data: Option<Value>,
) -> std::result::Result<(), String> {
    state
        .record_event(
            &run_id,
            &severity,
            &category,
            &stage,
            &message,
            data.unwrap_or_else(|| json!({})),
        )
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn diagnostics_upsert_item(
    state: State<'_, DiagnosticsState>,
    run_id: String,
    item_id: String,
    label: String,
    status: String,
    data: Option<Value>,
    error: Option<Value>,
) -> std::result::Result<(), String> {
    state
        .upsert_item(
            &run_id,
            &item_id,
            &label,
            &status,
            data.unwrap_or_else(|| json!({})),
            error,
        )
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn diagnostics_finish_run(
    state: State<'_, DiagnosticsState>,
    run_id: String,
    outcome: String,
    result: Option<Value>,
) -> std::result::Result<Option<DiagnosticReportPaths>, String> {
    state
        .finish_run(&run_id, &outcome, result.unwrap_or_else(|| json!({})))
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn get_latest_diagnostic_report(state: State<'_, DiagnosticsState>) -> Option<DiagnosticReportPaths> {
    let json_path = state.latest_report_path()?;
    let markdown_path = json_path.with_extension("md");
    Some(DiagnosticReportPaths {
        json: sanitize_path_string(&json_path),
        markdown: sanitize_path_string(&markdown_path),
    })
}

#[tauri::command]
pub fn get_latest_diagnostic_report_content(state: State<'_, DiagnosticsState>) -> std::result::Result<Option<String>, String> {
    let Some(path) = state.latest_report_path() else {
        return Ok(None);
    };
    fs::read_to_string(path).map(Some).map_err(|error| error.to_string())
}

#[tauri::command]
pub fn show_diagnostics_folder(state: State<'_, DiagnosticsState>) -> std::result::Result<(), String> {
    showfile::show_path_in_file_manager(state.root()).map_err(|error| error.to_string())
}

#[tauri::command]
pub fn show_latest_diagnostic_report(state: State<'_, DiagnosticsState>) -> std::result::Result<bool, String> {
    let Some(path) = state.latest_report_path() else {
        return Ok(false);
    };
    showfile::show_path_in_file_manager(path).map_err(|error| error.to_string())?;
    Ok(true)
}

pub fn input_file_metadata(path: &Path) -> Value {
    let metadata = fs::metadata(path).ok();
    json!({
        "path": sanitize_path_string(path),
        "file_name": path.file_name().map(|value| value.to_string_lossy().to_string()),
        "extension": path.extension().map(|value| value.to_string_lossy().to_lowercase()),
        "size_bytes": metadata.as_ref().map(std::fs::Metadata::len),
        "exists": path.exists(),
        "is_file": path.is_file(),
    })
}

pub fn analyze_transcript(segments: &[Segment], media_duration_seconds: Option<f64>) -> (Value, Vec<DiagnosticAnomaly>) {
    let mut words = 0usize;
    let mut characters = 0usize;
    let mut invalid_timestamp_segments = 0usize;
    let mut out_of_order_segments = 0usize;
    let mut largest_gap_cs = 0i64;
    let mut adjacent_exact_duplicates = 0usize;
    let mut normalized_counts: HashMap<String, usize> = HashMap::new();
    let mut previous_start = None;
    let mut previous_stop = None;
    let mut intervals: Vec<(i64, i64)> = Vec::new();
    let mut previous_text = String::new();

    for segment in segments {
        let normalized = normalize_text(&segment.text);
        words += normalized.split_whitespace().count();
        characters += normalized.chars().filter(|ch| !ch.is_whitespace()).count();
        if segment.start < 0 || segment.stop < segment.start {
            invalid_timestamp_segments += 1;
        } else {
            intervals.push((segment.start, segment.stop));
        }
        if let Some(start) = previous_start {
            if segment.start < start {
                out_of_order_segments += 1;
            }
        }
        if let Some(stop) = previous_stop {
            if segment.start > stop {
                largest_gap_cs = largest_gap_cs.max(segment.start - stop);
            }
        }
        if !normalized.is_empty() {
            if normalized == previous_text {
                adjacent_exact_duplicates += 1;
            }
            *normalized_counts.entry(normalized.clone()).or_default() += 1;
            previous_text = normalized;
        }
        previous_start = Some(segment.start);
        previous_stop = Some(segment.stop.max(previous_stop.unwrap_or_default()));
    }

    intervals.sort_by_key(|interval| interval.0);
    let mut covered_cs = 0i64;
    let mut current: Option<(i64, i64)> = None;
    for (start, stop) in intervals {
        current = match current {
            None => Some((start, stop)),
            Some((current_start, current_stop)) if start <= current_stop => Some((current_start, current_stop.max(stop))),
            Some((current_start, current_stop)) => {
                covered_cs += (current_stop - current_start).max(0);
                Some((start, stop))
            }
        };
    }
    if let Some((start, stop)) = current {
        covered_cs += (stop - start).max(0);
    }

    let dominant = normalized_counts
        .iter()
        .max_by_key(|(_, count)| **count)
        .map(|(text, count)| (text.clone(), *count));
    let transcript_end_cs = segments.iter().map(|segment| segment.stop).max().unwrap_or_default();
    let duration_seconds = media_duration_seconds.unwrap_or(transcript_end_cs.max(0) as f64 / 100.0);
    let coverage_ratio = if duration_seconds > 0.0 {
        (covered_cs as f64 / 100.0 / duration_seconds).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let words_per_minute = if duration_seconds > 0.0 {
        words as f64 / (duration_seconds / 60.0)
    } else {
        0.0
    };
    let adjacent_duplicate_ratio = if segments.len() > 1 {
        adjacent_exact_duplicates as f64 / (segments.len() - 1) as f64
    } else {
        0.0
    };

    let metrics = json!({
        "accuracy_ground_truth_available": false,
        "accuracy_note": "These are structural anomaly signals, not WER/CER or a measured accuracy score.",
        "segment_count": segments.len(),
        "word_count": words,
        "character_count": characters,
        "duration_seconds": duration_seconds,
        "transcript_end_seconds": transcript_end_cs as f64 / 100.0,
        "covered_audio_seconds": covered_cs as f64 / 100.0,
        "temporal_coverage_ratio": coverage_ratio,
        "words_per_minute": words_per_minute,
        "largest_gap_seconds": largest_gap_cs as f64 / 100.0,
        "invalid_timestamp_segments": invalid_timestamp_segments,
        "out_of_order_segments": out_of_order_segments,
        "adjacent_exact_duplicates": adjacent_exact_duplicates,
        "adjacent_exact_duplicate_ratio": adjacent_duplicate_ratio,
        "dominant_repeated_segment_count": dominant.as_ref().map(|(_, count)| *count).unwrap_or_default(),
        "dominant_repeated_segment_excerpt": dominant.as_ref().filter(|(_, count)| *count >= 3).map(|(text, _)| truncate(text, 160)),
    });

    let mut anomalies = Vec::new();
    if segments.is_empty() {
        anomalies.push(anomaly("empty_transcript", "warning", "Transcription completed without any segments.", json!({})));
    }
    if invalid_timestamp_segments > 0 {
        anomalies.push(anomaly(
            "invalid_timestamps",
            "error",
            "One or more transcript segments contain invalid timestamps.",
            json!({ "count": invalid_timestamp_segments }),
        ));
    }
    if out_of_order_segments > 0 {
        anomalies.push(anomaly(
            "out_of_order_timestamps",
            "error",
            "Transcript segment timestamps moved backwards.",
            json!({ "count": out_of_order_segments }),
        ));
    }
    if duration_seconds >= 30.0 && largest_gap_cs >= 1_500 {
        anomalies.push(anomaly(
            "large_temporal_gap",
            "warning",
            "A gap of at least 15 seconds exists between transcript segments. This can be legitimate silence, but is worth inspecting when speech is known to exist there.",
            json!({ "largest_gap_seconds": largest_gap_cs as f64 / 100.0 }),
        ));
    }
    if duration_seconds >= 30.0 && coverage_ratio < 0.20 && !segments.is_empty() {
        anomalies.push(anomaly(
            "low_temporal_coverage",
            "warning",
            "Transcript segments cover less than 20% of the media timeline. This is a diagnostic signal, not proof of missing speech.",
            json!({ "temporal_coverage_ratio": coverage_ratio }),
        ));
    }
    if adjacent_duplicate_ratio >= 0.35 && adjacent_exact_duplicates >= 3 {
        anomalies.push(anomaly(
            "high_adjacent_repetition",
            "warning",
            "Many consecutive transcript segments contain identical text. This can indicate decoder degeneration or legitimate repeated speech; audio/reference inspection is required.",
            json!({
                "adjacent_exact_duplicates": adjacent_exact_duplicates,
                "ratio": adjacent_duplicate_ratio,
            }),
        ));
    }
    if let Some((text, count)) = dominant {
        if count >= 5 && count * 2 >= segments.len().max(1) {
            anomalies.push(anomaly(
                "dominant_repeated_segment",
                "warning",
                "One text segment dominates a large share of the output. Treat this as a repetition signal, not automatic hallucination.",
                json!({ "count": count, "excerpt": truncate(&text, 160) }),
            ));
        }
    }

    (metrics, anomalies)
}

pub fn sanitize_value(value: Value) -> Value {
    sanitize_value_with_key(value, None)
}

fn sanitize_value_with_key(value: Value, key: Option<&str>) -> Value {
    if key.is_some_and(is_secret_key) {
        return Value::String("<redacted>".to_string());
    }
    match value {
        Value::Object(map) => {
            let mut sanitized = Map::with_capacity(map.len());
            for (child_key, child_value) in map {
                sanitized.insert(
                    child_key.clone(),
                    sanitize_value_with_key(child_value, Some(&child_key)),
                );
            }
            Value::Object(sanitized)
        }
        Value::Array(values) => Value::Array(values.into_iter().map(|value| sanitize_value_with_key(value, key)).collect()),
        Value::String(value) => {
            let value = redact_home(&value);
            if key.is_some_and(|key| key.to_ascii_lowercase().contains("prompt")) && value.len() > 500 {
                Value::String(format!("{}… <truncated>", truncate(&value, 500)))
            } else {
                Value::String(value)
            }
        }
        other => other,
    }
}

fn app_snapshot(app: &tauri::AppHandle) -> Value {
    use tauri_plugin_os::{arch, platform, type_, version};
    json!({
        "name": app.package_info().name.to_string(),
        "version": app.package_info().version.to_string(),
        "commit_hash": get_commit_hash(),
        "architecture": arch(),
        "platform": platform(),
        "os_type": type_(),
        "os_version": version(),
        "rust_target_arch": std::env::consts::ARCH,
        "avx2": is_avx2_enabled(),
    })
}

pub fn get_app_info() -> String {
    use tauri_plugin_os::{arch, platform, type_, version};
    format!(
        "Commit Hash: {}\nArch: {}\nPlatform: {}\nOS: {}\nOS Version: {}\nAVX2: {}",
        get_commit_hash(),
        arch(),
        platform(),
        type_(),
        version(),
        is_avx2_enabled()
    )
}

pub fn get_issue_url(logs: String) -> String {
    let extra_info = get_app_info();
    format!(
        "https://github.com/thewh1teagle/vibe/issues/new?assignees=octocat&labels=bug&projects=&template=bug_report.yaml&title=App+reports+bug&logs={}",
        urlencoding::encode(&format!("{}\n\n{}", extra_info, logs))
    )
}

fn persist_run(run: &DiagnosticRun, include_markdown: bool) -> Result<()> {
    fs::create_dir_all(&run.report_dir).context("failed to create diagnostics report directory")?;
    let json_path = run.report_dir.join(format!("{}.json", run.run_id));
    let tmp_path = run.report_dir.join(format!("{}.json.tmp", run.run_id));
    let serialized = serde_json::to_vec_pretty(run).context("failed to serialize diagnostic report")?;
    fs::write(&tmp_path, serialized).context("failed to write diagnostic report snapshot")?;
    if json_path.exists() {
        fs::remove_file(&json_path).context("failed to replace previous diagnostic report snapshot")?;
    }
    fs::rename(&tmp_path, &json_path).context("failed to commit diagnostic report snapshot")?;
    if include_markdown {
        let markdown_path = run.report_dir.join(format!("{}.md", run.run_id));
        fs::write(markdown_path, render_markdown(run)).context("failed to write diagnostic markdown report")?;
    }
    Ok(())
}

fn render_markdown(run: &DiagnosticRun) -> String {
    let mut output = String::new();
    output.push_str("# Vibe Diagnostic Report\n\n");
    output.push_str(&format!("- **Run ID:** `{}`\n", run.run_id));
    output.push_str(&format!("- **Source:** `{}`\n", run.source));
    output.push_str(&format!("- **Outcome:** `{}`\n", run.status));
    output.push_str(&format!("- **Started:** {}\n", run.started_at));
    output.push_str(&format!("- **Finished:** {}\n", run.finished_at.as_deref().unwrap_or("not finalized")));
    output.push_str(&format!("- **Duration:** {} ms\n\n", run.duration_ms.unwrap_or_default()));

    output.push_str("## Environment\n\n```json\n");
    output.push_str(&serde_json::to_string_pretty(&run.app).unwrap_or_default());
    output.push_str("\n```\n\n## Context\n\n```json\n");
    output.push_str(&serde_json::to_string_pretty(&run.context).unwrap_or_default());
    output.push_str("\n```\n\n");

    output.push_str("## Anomalies\n\n");
    if run.anomalies.is_empty() {
        output.push_str("No anomaly signals were recorded.\n\n");
    } else {
        for item in &run.anomalies {
            output.push_str(&format!("- **{}** (`{}`): {}\n", item.severity, item.code, item.message));
        }
        output.push('\n');
    }

    output.push_str("## Batch / Items\n\n");
    if run.items.is_empty() {
        output.push_str("No child items were recorded.\n\n");
    } else {
        output.push_str("| Item | Status | Started | Finished |\n|---|---|---|---|\n");
        for item in &run.items {
            output.push_str(&format!(
                "| {} | `{}` | {} | {} |\n",
                markdown_cell(&item.label),
                item.status,
                item.started_at.as_deref().unwrap_or(""),
                item.finished_at.as_deref().unwrap_or("")
            ));
        }
        output.push('\n');
    }

    output.push_str("## Event Timeline\n\n");
    output.push_str("| Time | Severity | Stage | Message |\n|---|---|---|---|\n");
    for event in &run.events {
        output.push_str(&format!(
            "| {} | {} | `{}` | {} |\n",
            event.timestamp,
            event.severity,
            event.stage,
            markdown_cell(&event.message)
        ));
    }

    output.push_str("\n## Result\n\n```json\n");
    output.push_str(&serde_json::to_string_pretty(&run.result).unwrap_or_default());
    output.push_str("\n```\n\n## Evidence & privacy\n\n```json\n");
    output.push_str(&serde_json::to_string_pretty(&json!({ "evidence": run.evidence, "privacy": run.privacy })).unwrap_or_default());
    output.push_str("\n```\n");
    output
}

fn anomaly(code: &str, severity: &str, message: &str, evidence: Value) -> DiagnosticAnomaly {
    DiagnosticAnomaly {
        code: code.to_string(),
        severity: severity.to_string(),
        message: message.to_string(),
        evidence,
    }
}

fn is_terminal_status(status: &str) -> bool {
    matches!(status, "succeeded" | "failed" | "aborted" | "skipped")
}

fn is_secret_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase();
    ["api_key", "apikey", "token", "secret", "password", "authorization", "bearer"]
        .iter()
        .any(|needle| normalized.contains(needle))
}

fn sanitize_path_string(path: &Path) -> String {
    redact_home(&path.to_string_lossy())
}

fn redact_home(value: &str) -> String {
    let home = std::env::var("USERPROFILE").ok().or_else(|| std::env::var("HOME").ok());
    match home {
        Some(home) if !home.is_empty() => value.replace(&home, "$HOME"),
        _ => value.to_string(),
    }
}

fn normalize_text(text: &str) -> String {
    let mut result = String::new();
    let mut previous_space = false;
    for ch in text.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            result.push(ch);
            previous_space = false;
        } else if !previous_space && !result.is_empty() {
            result.push(' ');
            previous_space = true;
        }
    }
    result.trim().to_string()
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn markdown_cell(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', " ").replace('\r', " ")
}

fn newest_report(root: &Path) -> Option<PathBuf> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for day in fs::read_dir(root).ok()?.flatten() {
        if !day.path().is_dir() {
            continue;
        }
        for file in fs::read_dir(day.path()).ok()?.flatten() {
            let path = file.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let modified = file.metadata().ok()?.modified().ok()?;
            if newest.as_ref().is_none_or(|(current, _)| modified > *current) {
                newest = Some((modified, path));
            }
        }
    }
    newest.map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(start: i64, stop: i64, text: &str) -> Segment {
        Segment {
            start,
            stop,
            text: text.to_string(),
            speaker: None,
        }
    }

    #[test]
    fn sanitizer_redacts_secrets_and_home_paths() {
        let home = std::env::var("USERPROFILE").ok().or_else(|| std::env::var("HOME").ok());
        let path = home.clone().map(|home| format!("{home}/audio.wav")).unwrap_or_else(|| "/tmp/audio.wav".to_string());
        let value = sanitize_value(json!({
            "api_key": "super-secret",
            "nested": { "authorization": "Bearer abc", "path": path }
        }));
        assert_eq!(value["api_key"], "<redacted>");
        assert_eq!(value["nested"]["authorization"], "<redacted>");
        if home.is_some() {
            assert!(value["nested"]["path"].as_str().unwrap().contains("$HOME"));
        }
    }

    #[test]
    fn transcript_analysis_detects_timestamp_and_repetition_signals() {
        let segments = vec![
            segment(0, 100, "frase repetida"),
            segment(100, 200, "frase repetida"),
            segment(200, 300, "frase repetida"),
            segment(300, 400, "frase repetida"),
            segment(400, 500, "frase repetida"),
        ];
        let (metrics, anomalies) = analyze_transcript(&segments, Some(5.0));
        assert_eq!(metrics["adjacent_exact_duplicates"], 4);
        assert!(anomalies.iter().any(|item| item.code == "high_adjacent_repetition"));
        assert!(anomalies.iter().any(|item| item.code == "dominant_repeated_segment"));
    }

    #[test]
    fn transcript_analysis_does_not_call_repetition_inaccuracy() {
        let segments = vec![
            segment(0, 100, "muito obrigado"),
            segment(100, 200, "muito obrigado"),
            segment(200, 300, "muito obrigado"),
            segment(300, 400, "muito obrigado"),
            segment(400, 500, "muito obrigado"),
        ];
        let (metrics, anomalies) = analyze_transcript(&segments, Some(5.0));
        assert_eq!(metrics["accuracy_ground_truth_available"], false);
        assert!(anomalies.iter().all(|item| !item.message.to_ascii_lowercase().contains("inaccurate")));
    }

    #[test]
    fn transcript_analysis_detects_invalid_order() {
        let segments = vec![segment(200, 300, "dois"), segment(100, 150, "um")];
        let (_, anomalies) = analyze_transcript(&segments, Some(3.0));
        assert!(anomalies.iter().any(|item| item.code == "out_of_order_timestamps"));
    }

    #[test]
    fn terminal_item_statuses_are_explicit() {
        assert!(is_terminal_status("succeeded"));
        assert!(is_terminal_status("failed"));
        assert!(is_terminal_status("aborted"));
        assert!(is_terminal_status("skipped"));
        assert!(!is_terminal_status("running"));
    }

    #[test]
    fn normalized_text_is_stable_for_diagnostic_comparisons() {
        let variants: HashSet<String> = ["Olá, MUNDO!", "olá mundo", "Olá   mundo"]
            .into_iter()
            .map(normalize_text)
            .collect();
        assert_eq!(variants.len(), 1);
    }
}
