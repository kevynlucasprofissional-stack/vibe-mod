use crate::ffmpeg::{find_ffmpeg_path, get_vibe_temp_folder, random_string};
use crate::transcript::Segment;
use eyre::{bail, Context, ContextCompat, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

pub const DEFAULT_CHUNK_SECONDS: f64 = 30.0;
pub const DEFAULT_OVERLAP_SECONDS: f64 = 1.0;
pub const CHUNKING_MIN_DURATION_SECONDS: f64 = 35.0;
pub const MIN_RETRY_CHUNK_SECONDS: f64 = 7.5;
pub const MAX_RETRY_DEPTH: u8 = 2;
const SILENCE_LOOKBACK_SECONDS: f64 = 2.0;
const SILENCE_NOISE_DB: &str = "-35dB";
const SILENCE_MIN_SECONDS: &str = "0.25";
const REPETITION_THRESHOLD: f64 = 0.65;

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
    while duration - owner_start > DEFAULT_CHUNK_SECONDS {
        let target = (owner_start + DEFAULT_CHUNK_SECONDS).min(duration);
        let minimum_cut = (target - SILENCE_LOOKBACK_SECONDS).max(owner_start + 1.0);
        let owner_end = best_silence_cut(silences, minimum_cut, target).unwrap_or(target);
        let owner_end = if owner_end <= owner_start + 0.1 { target } else { owner_end };
        windows.push(make_window(owner_start, owner_end, duration, 0));
        owner_start = owner_end;
    }
    if owner_start < duration {
        windows.push(make_window(owner_start, duration, duration, 0));
    }
    windows
}

pub fn split_window(window: ChunkWindow, media_duration: f64) -> Option<(ChunkWindow, ChunkWindow)> {
    if window.owner_duration() <= MIN_RETRY_CHUNK_SECONDS || window.depth >= MAX_RETRY_DEPTH {
        return None;
    }
    let midpoint = window.owner_start + window.owner_duration() / 2.0;
    let next_depth = window.depth + 1;
    Some((
        make_window(window.owner_start, midpoint, media_duration, next_depth),
        make_window(midpoint, window.owner_end, media_duration, next_depth),
    ))
}

fn make_window(owner_start: f64, owner_end: f64, media_duration: f64, depth: u8) -> ChunkWindow {
    ChunkWindow {
        owner_start,
        owner_end,
        extract_start: (owner_start - DEFAULT_OVERLAP_SECONDS).max(0.0),
        extract_end: (owner_end + DEFAULT_OVERLAP_SECONDS).min(media_duration),
        depth,
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
    let ffmpeg = find_ffmpeg_path().context("ffmpeg not found")?;
    let output_path = get_vibe_temp_folder().join(format!("chunk_{}.wav", random_string(12)));
    let start = format!("{:.3}", window.extract_start);
    let duration = format!("{:.3}", (window.extract_end - window.extract_start).max(0.01));

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
        bail!("ffmpeg chunk extraction failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(output_path)
}

pub fn globalize_segments(segments: Vec<Segment>, window: ChunkWindow, media_duration: f64) -> Vec<Segment> {
    let offset = (window.extract_start * 100.0).round() as i64;
    let owner_start = (window.owner_start * 100.0).round() as i64;
    let owner_end = (window.owner_end * 100.0).round() as i64;
    let media_end = (media_duration * 100.0).round() as i64;

    segments
        .into_iter()
        .filter_map(|mut segment| {
            segment.start = (segment.start + offset).clamp(0, media_end);
            segment.stop = (segment.stop + offset).clamp(segment.start, media_end);
            let midpoint = segment.start + (segment.stop - segment.start) / 2;
            if midpoint < owner_start || midpoint >= owner_end {
                return None;
            }
            Some(segment)
        })
        .collect()
}

pub fn repetition_score(segments: &[Segment]) -> f64 {
    let normalized: Vec<String> = segments
        .iter()
        .map(|segment| normalize_text(&segment.text))
        .filter(|text| !text.is_empty())
        .collect();
    if normalized.len() < 3 {
        return 0.0;
    }

    let adjacent_duplicates = normalized.windows(2).filter(|pair| pair[0] == pair[1] && pair[0].len() >= 6).count();
    let adjacent_ratio = adjacent_duplicates as f64 / (normalized.len() - 1) as f64;

    let mut frequencies: HashMap<&str, usize> = HashMap::new();
    for text in &normalized {
        if text.len() >= 6 {
            *frequencies.entry(text.as_str()).or_default() += 1;
        }
    }
    let dominant_count = frequencies.values().copied().max().unwrap_or(0);
    let dominant_ratio = if dominant_count >= 3 {
        dominant_count as f64 / normalized.len() as f64
    } else {
        0.0
    };

    let tokens: Vec<&str> = normalized.iter().flat_map(|text| text.split_whitespace()).collect();
    let ngram_ratio = if tokens.len() >= 12 {
        let trigrams: Vec<String> = tokens.windows(3).map(|w| w.join(" ")).collect();
        let unique = trigrams.iter().collect::<HashSet<_>>().len();
        1.0 - unique as f64 / trigrams.len() as f64
    } else {
        0.0
    };

    adjacent_ratio.max(dominant_ratio).max(ngram_ratio)
}

pub fn is_repetition_suspicious(segments: &[Segment]) -> bool {
    repetition_score(segments) >= REPETITION_THRESHOLD
}

pub fn sanitize_pathological_repetitions(segments: Vec<Segment>) -> Vec<Segment> {
    let mut cleaned: Vec<Segment> = Vec::with_capacity(segments.len());
    for segment in segments {
        let duplicate = cleaned.last().is_some_and(|previous| {
            normalize_text(&previous.text).len() >= 6
                && text_similarity(&previous.text, &segment.text) >= 0.94
                && segment.start.saturating_sub(previous.stop) <= 100
        });
        if !duplicate {
            cleaned.push(segment);
        }
    }
    cleaned
}

pub fn merge_segments(mut segments: Vec<Segment>) -> Vec<Segment> {
    segments.sort_by_key(|segment| (segment.start, segment.stop));
    let mut merged: Vec<Segment> = Vec::with_capacity(segments.len());
    for segment in segments {
        let duplicate = merged.last().is_some_and(|previous| {
            let close_in_time = segment.start <= previous.stop.saturating_add(150);
            close_in_time && text_similarity(&previous.text, &segment.text) >= 0.90
        });
        if !duplicate {
            merged.push(segment);
        }
    }
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

fn text_similarity(a: &str, b: &str) -> f64 {
    let a = normalize_text(a);
    let b = normalize_text(b);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    if (a.contains(&b) || b.contains(&a)) && a.len().min(b.len()) >= 12 {
        return a.len().min(b.len()) as f64 / a.len().max(b.len()) as f64;
    }
    let a_tokens: HashSet<&str> = a.split_whitespace().collect();
    let b_tokens: HashSet<&str> = b.split_whitespace().collect();
    let union = a_tokens.union(&b_tokens).count();
    if union == 0 {
        return 0.0;
    }
    a_tokens.intersection(&b_tokens).count() as f64 / union as f64
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
                    ranges.push(SilenceRange { start, end: value.min(duration) });
                }
            }
        }
    }
    if let Some(start) = current_start {
        if duration > start {
            ranges.push(SilenceRange { start, end: duration });
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
        Segment { start, stop, text: text.to_string(), speaker: None }
    }

    #[test]
    fn parses_ffmpeg_duration() {
        let stderr = "Input #0\n  Duration: 01:02:03.50, start: 0.000000, bitrate: 128 kb/s";
        assert_eq!(parse_duration(stderr), Some(3723.5));
    }

    #[test]
    fn plans_chunks_at_or_below_thirty_seconds() {
        let windows = plan_chunks(95.0, &[]);
        assert_eq!(windows.len(), 4);
        assert!(windows.iter().all(|window| window.owner_duration() <= 30.0));
        assert_eq!(windows.last().unwrap().owner_end, 95.0);
    }

    #[test]
    fn prefers_silence_before_target_without_exceeding_thirty_seconds() {
        let windows = plan_chunks(70.0, &[SilenceRange { start: 28.8, end: 29.4 }]);
        assert!((windows[0].owner_end - 29.4).abs() < f64::EPSILON);
        assert!(windows[0].owner_duration() <= 30.0);
    }

    #[test]
    fn overlap_does_not_change_ownership() {
        let window = plan_chunks(65.0, &[])[1];
        assert!(window.extract_start < window.owner_start);
        assert!(window.extract_end > window.owner_end);
    }

    #[test]
    fn globalizes_and_keeps_only_owned_segments() {
        let window = ChunkWindow { owner_start: 30.0, owner_end: 60.0, extract_start: 29.0, extract_end: 61.0, depth: 0 };
        let local = vec![segment(50, 150, "left overlap"), segment(200, 300, "owned"), segment(3100, 3200, "right overlap")];
        let global = globalize_segments(local, window, 90.0);
        assert_eq!(global.len(), 1);
        assert_eq!(global[0].start, 3100);
        assert_eq!(global[0].stop, 3200);
    }

    #[test]
    fn detects_pathological_repetition() {
        let segments = vec![
            segment(0, 100, "this phrase is repeating forever"),
            segment(100, 200, "this phrase is repeating forever"),
            segment(200, 300, "this phrase is repeating forever"),
            segment(300, 400, "this phrase is repeating forever"),
        ];
        assert!(is_repetition_suspicious(&segments));
    }

    #[test]
    fn healthy_transcript_is_not_flagged() {
        let segments = vec![
            segment(0, 100, "welcome to the meeting today"),
            segment(100, 200, "we will review the quarterly results"),
            segment(200, 300, "after that maria will discuss hiring"),
            segment(300, 400, "finally we will answer questions"),
        ];
        assert!(!is_repetition_suspicious(&segments));
    }

    #[test]
    fn merge_removes_boundary_duplicates() {
        let merged = merge_segments(vec![
            segment(2900, 3100, "we need to finish this sentence"),
            segment(3000, 3200, "we need to finish this sentence"),
            segment(3200, 3400, "and then continue"),
        ]);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn suspicious_chunk_can_be_split_until_retry_floor() {
        let window = plan_chunks(60.0, &[])[0];
        let (left, right) = split_window(window, 60.0).unwrap();
        assert!((left.owner_duration() - 15.0).abs() < 0.001);
        assert!((right.owner_duration() - 15.0).abs() < 0.001);
        let (quarter, _) = split_window(left, 60.0).unwrap();
        assert!(split_window(quarter, 60.0).is_none());
    }
}
