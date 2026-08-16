use crate::ffmpeg::{find_ffmpeg_path, get_vibe_temp_folder, random_string};
use crate::transcript::Segment;
use eyre::{bail, Context, ContextCompat, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Hard ceiling for every normal transcription request sent to Sona.
pub const DEFAULT_CHUNK_SECONDS: f64 = 30.0;
pub const DEFAULT_OVERLAP_SECONDS: f64 = 1.0;
pub const CHUNKING_MIN_DURATION_SECONDS: f64 = DEFAULT_CHUNK_SECONDS;
const BASE_OWNER_SECONDS: f64 = DEFAULT_CHUNK_SECONDS - (DEFAULT_OVERLAP_SECONDS * 2.0);
const SILENCE_LOOKBACK_SECONDS: f64 = 2.0;
const SILENCE_NOISE_DB: &str = "-35dB";
const SILENCE_MIN_SECONDS: &str = "0.25";

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SilenceRange {
    pub start: f64,
    pub end: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChunkWindow {
    pub owner_start: f64,
    pub owner_end: f64,
    pub extract_start: f64,
    pub extract_end: f64,
    pub depth: u8,
}

impl ChunkWindow {
    pub fn owner_duration(&self) -> f64 {
        (self.owner_end - self.owner_start).max(0.0)
    }

    pub fn extract_duration(&self) -> f64 {
        (self.extract_end - self.extract_start).max(0.0)
    }
}

fn configure_command(cmd: &mut Command) {
    cmd.stdin(Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
}

pub fn probe_duration_seconds(input: &Path) -> Result<f64> {
    let ffmpeg = find_ffmpeg_path().context("ffmpeg not found")?;
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-i"]).arg(input);
    configure_command(&mut cmd);
    let output = cmd.output().context("failed to probe media duration with ffmpeg")?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_duration(&stderr).context("ffmpeg did not report a finite media duration")
}

pub fn detect_silences(input: &Path, duration: f64) -> Result<Vec<SilenceRange>> {
    let ffmpeg = find_ffmpeg_path().context("ffmpeg not found")?;
    let filter = format!("silencedetect=noise={SILENCE_NOISE_DB}:d={SILENCE_MIN_SECONDS}");
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostats", "-i"])
        .arg(input)
        .args(["-vn", "-sn", "-dn", "-af", &filter, "-f", "null", "-"]);
    configure_command(&mut cmd);
    let output = cmd.output().context("failed to analyze silence with ffmpeg")?;
    if !output.status.success() {
        bail!("ffmpeg silence analysis failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(parse_silences(&String::from_utf8_lossy(&output.stderr), duration))
}

pub fn plan_chunks(duration: f64, silences: &[SilenceRange]) -> Vec<ChunkWindow> {
    if duration <= 0.0 {
        return Vec::new();
    }

    let mut windows = Vec::new();
    let mut owner_start = 0.0;
    while duration - owner_start > BASE_OWNER_SECONDS {
        let target = (owner_start + BASE_OWNER_SECONDS).min(duration);
        let minimum_cut = (target - SILENCE_LOOKBACK_SECONDS).max(owner_start + 1.0);
        let owner_end = best_silence_cut(silences, minimum_cut, target).unwrap_or(target);
        let owner_end = if owner_end <= owner_start + 0.1 { target } else { owner_end };
        windows.push(make_window(owner_start, owner_end, duration));
        owner_start = owner_end;
    }
    if owner_start < duration {
        windows.push(make_window(owner_start, duration, duration));
    }
    windows
}

fn make_window(owner_start: f64, owner_end: f64, media_duration: f64) -> ChunkWindow {
    ChunkWindow {
        owner_start,
        owner_end,
        extract_start: (owner_start - DEFAULT_OVERLAP_SECONDS).max(0.0),
        extract_end: (owner_end + DEFAULT_OVERLAP_SECONDS).min(media_duration),
        depth: 0,
    }
}

fn best_silence_cut(silences: &[SilenceRange], minimum_cut: f64, target: f64) -> Option<f64> {
    let mut best = None;
    for silence in silences {
        let candidate = if silence.start <= target && silence.end >= target {
            Some(target)
        } else if silence.end <= target && silence.end >= minimum_cut {
            Some(silence.end)
        } else if silence.start <= target && silence.start >= minimum_cut {
            Some(silence.start)
        } else {
            None
        };
        if let Some(candidate) = candidate {
            if best.is_none_or(|current| candidate > current) {
                best = Some(candidate);
            }
        }
    }
    best
}

pub fn extract_chunk(input: &Path, window: ChunkWindow) -> Result<PathBuf> {
    if window.extract_duration() > DEFAULT_CHUNK_SECONDS + 0.001 {
        bail!(
            "planned transcription chunk exceeds {:.1}s ceiling: {:.3}s",
            DEFAULT_CHUNK_SECONDS,
            window.extract_duration()
        );
    }

    let ffmpeg = find_ffmpeg_path().context("ffmpeg not found")?;
    let output_path = get_vibe_temp_folder().join(format!("chunk_{}.wav", random_string(12)));
    let start = format!("{:.3}", window.extract_start);
    let duration = format!("{:.3}", window.extract_duration().max(0.01));

    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-ss", &start, "-i"])
        .arg(input)
        .args([
            "-t",
            &duration,
            "-vn",
            "-sn",
            "-dn",
            "-ar",
            "16000",
            "-ac",
            "1",
            "-c:a",
            "pcm_s16le",
            "-y",
        ])
        .arg(&output_path);
    configure_command(&mut cmd);
    let output = cmd.output().context("failed to extract transcription chunk")?;
    if !output.status.success() || !output_path.exists() {
        let _ = std::fs::remove_file(&output_path);
        bail!("ffmpeg chunk extraction failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(output_path)
}

/// Convert chunk-local timestamps back to the source-media timeline.
///
/// Every segment produced in the overlap is preserved. Boundary reconciliation
/// happens later in `merge_segments` so a phrase cannot disappear merely because
/// neighboring requests segmented the same acoustic event differently.
pub fn globalize_segments(segments: Vec<Segment>, window: ChunkWindow, media_duration: f64) -> Vec<Segment> {
    let offset = (window.extract_start * 100.0).round() as i64;
    let media_end = (media_duration * 100.0).round() as i64;

    segments
        .into_iter()
        .map(|mut segment| {
            segment.start = (segment.start + offset).clamp(0, media_end);
            segment.stop = (segment.stop + offset).clamp(segment.start, media_end);
            segment
        })
        .collect()
}

/// Merge only boundary duplicates for which there is strong evidence that both
/// requests describe the same acoustic event: equal normalized text, equal
/// speaker identity, and a real positive temporal overlap.
///
/// Ambiguous variants are deliberately preserved. This function never attempts
/// to identify or delete model repetition loops.
pub fn merge_segments(mut segments: Vec<Segment>) -> Vec<Segment> {
    segments.sort_by_key(|segment| (segment.start, segment.stop));
    let mut merged: Vec<Segment> = Vec::with_capacity(segments.len());

    for segment in segments {
        let normalized = normalize_text(&segment.text);
        let duplicate_index = if normalized.is_empty() {
            None
        } else {
            merged.iter().rposition(|previous| {
                let overlaps = segment.start < previous.stop && previous.start < segment.stop;
                overlaps
                    && previous.speaker == segment.speaker
                    && normalize_text(&previous.text) == normalized
            })
        };

        if let Some(index) = duplicate_index {
            let previous = &mut merged[index];
            previous.start = previous.start.min(segment.start);
            previous.stop = previous.stop.max(segment.stop);
        } else {
            merged.push(segment);
        }
    }

    merged.sort_by_key(|segment| (segment.start, segment.stop));
    merged
}

fn normalize_text(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut previous_space = false;
    for ch in text.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            normalized.push(ch);
            previous_space = false;
        } else if !previous_space && !normalized.is_empty() {
            normalized.push(' ');
            previous_space = true;
        }
    }
    normalized.trim().to_string()
}

fn parse_duration(stderr: &str) -> Option<f64> {
    let marker = "Duration: ";
    let start = stderr.find(marker)? + marker.len();
    let value = stderr[start..].split(',').next()?.trim();
    if value == "N/A" {
        return None;
    }
    let mut parts = value.split(':');
    let hours: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next()?.parse().ok()?;
    let seconds: f64 = parts.next()?.parse().ok()?;
    let duration = hours * 3600.0 + minutes * 60.0 + seconds;
    duration.is_finite().then_some(duration)
}

fn parse_silences(stderr: &str, duration: f64) -> Vec<SilenceRange> {
    let mut ranges = Vec::new();
    let mut current_start = None;
    for line in stderr.lines() {
        if let Some(value) = parse_metric(line, "silence_start:") {
            current_start = Some(value.max(0.0));
        }
        if let Some(value) = parse_metric(line, "silence_end:") {
            if let Some(start) = current_start.take() {
                if value > start {
                    ranges.push(SilenceRange {
                        start,
                        end: value.min(duration),
                    });
                }
            }
        }
    }
    if let Some(start) = current_start {
        if duration > start {
            ranges.push(SilenceRange {
                start,
                end: duration,
            });
        }
    }
    ranges
}

fn parse_metric(line: &str, marker: &str) -> Option<f64> {
    let start = line.find(marker)? + marker.len();
    line[start..].trim_start().split_whitespace().next()?.parse().ok()
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
    fn parses_ffmpeg_duration() {
        let stderr = "Input #0\n  Duration: 01:02:03.50, start: 0.000000, bitrate: 128 kb/s";
        assert_eq!(parse_duration(stderr), Some(3723.5));
    }

    #[test]
    fn every_model_request_is_at_most_thirty_seconds_including_overlap() {
        let windows = plan_chunks(95.0, &[]);
        assert_eq!(windows.len(), 4);
        assert!(windows
            .iter()
            .all(|window| window.extract_duration() <= DEFAULT_CHUNK_SECONDS + 0.001));
        assert_eq!(windows.last().unwrap().owner_end, 95.0);
    }

    #[test]
    fn prefers_silence_before_owner_target_without_breaking_request_ceiling() {
        let windows = plan_chunks(
            70.0,
            &[SilenceRange {
                start: 27.1,
                end: 27.7,
            }],
        );
        assert!((windows[0].owner_end - 27.7).abs() < f64::EPSILON);
        assert!(windows[0].extract_duration() <= DEFAULT_CHUNK_SECONDS);
    }

    #[test]
    fn overlap_does_not_change_ownership() {
        let window = plan_chunks(65.0, &[])[1];
        assert!(window.extract_start < window.owner_start);
        assert!(window.extract_end > window.owner_end);
        assert!(window.extract_duration() <= DEFAULT_CHUNK_SECONDS);
    }

    #[test]
    fn globalizes_overlap_segments_without_dropping_them() {
        let window = ChunkWindow {
            owner_start: 28.0,
            owner_end: 56.0,
            extract_start: 27.0,
            extract_end: 57.0,
            depth: 0,
        };
        let local = vec![
            segment(0, 50, "left overlap"),
            segment(200, 300, "owned"),
            segment(2900, 3000, "right overlap"),
        ];
        let global = globalize_segments(local, window, 90.0);
        assert_eq!(global.len(), 3);
        assert_eq!((global[0].start, global[0].stop), (2700, 2750));
        assert_eq!((global[1].start, global[1].stop), (2900, 3000));
        assert_eq!((global[2].start, global[2].stop), (5600, 5700));
    }

    #[test]
    fn merge_preserves_boundary_phrase_that_midpoint_ownership_could_drop() {
        let left_window = ChunkWindow {
            owner_start: 0.0,
            owner_end: 28.0,
            extract_start: 0.0,
            extract_end: 29.0,
            depth: 0,
        };
        let right_window = ChunkWindow {
            owner_start: 28.0,
            owner_end: 56.0,
            extract_start: 27.0,
            extract_end: 57.0,
            depth: 0,
        };

        let mut global = globalize_segments(
            vec![segment(2700, 2950, "frase que cruza a fronteira")],
            left_window,
            90.0,
        );
        global.extend(globalize_segments(
            vec![segment(0, 150, "frase que cruza a fronteira")],
            right_window,
            90.0,
        ));

        let merged = merge_segments(global);
        assert_eq!(merged.len(), 1);
        assert_eq!((merged[0].start, merged[0].stop), (2700, 2950));
    }

    #[test]
    fn merge_preserves_ambiguous_boundary_variants() {
        let merged = merge_segments(vec![
            segment(2700, 2950, "vamos começar agora"),
            segment(2700, 2850, "começar agora"),
        ]);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn merge_removes_exact_overlapping_boundary_duplicates() {
        let merged = merge_segments(vec![
            segment(2700, 2900, "we need to finish this sentence"),
            segment(2800, 3000, "we need to finish this sentence"),
            segment(3000, 3200, "and then continue"),
        ]);
        assert_eq!(merged.len(), 2);
        assert_eq!((merged[0].start, merged[0].stop), (2700, 3000));
    }

    #[test]
    fn merge_keeps_legitimate_repetition_when_intervals_do_not_overlap() {
        let merged = merge_segments(vec![
            segment(100, 150, "muito obrigado a todos"),
            segment(150, 200, "muito obrigado a todos"),
            segment(200, 250, "muito obrigado a todos"),
            segment(250, 300, "muito obrigado a todos"),
        ]);
        assert_eq!(merged.len(), 4);
    }
}
