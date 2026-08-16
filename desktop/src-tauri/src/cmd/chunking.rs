use crate::ffmpeg::{find_ffmpeg_path, get_vibe_temp_folder, random_string};
use crate::transcript::Segment;
use eyre::{bail, Context, ContextCompat, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Hard ceiling for every external Whisper request sent to Sona.
pub const DEFAULT_CHUNK_SECONDS: f64 = 30.0;
/// Shared audio between two neighboring requests. This preserves the effective
/// 2-second shared region from the first implementation without claiming that
/// the value is empirically optimal.
pub const DEFAULT_CHUNK_OVERLAP_SECONDS: f64 = 2.0;
pub const CHUNKING_MIN_DURATION_SECONDS: f64 = DEFAULT_CHUNK_SECONDS;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChunkWindow {
    pub start: f64,
    pub end: f64,
}

impl ChunkWindow {
    pub fn duration(&self) -> f64 {
        (self.end - self.start).max(0.0)
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

/// Plan fixed overlapping requests. There is deliberately no silence pre-pass:
/// correctness comes from full temporal coverage plus overlap, not from finding
/// a heuristic silence threshold.
pub fn plan_chunks(duration: f64) -> Vec<ChunkWindow> {
    if duration <= 0.0 {
        return Vec::new();
    }

    let stride = DEFAULT_CHUNK_SECONDS - DEFAULT_CHUNK_OVERLAP_SECONDS;
    debug_assert!(stride > 0.0);

    let mut windows = Vec::new();
    let mut start = 0.0;
    loop {
        let end = (start + DEFAULT_CHUNK_SECONDS).min(duration);
        windows.push(ChunkWindow { start, end });
        if end >= duration {
            break;
        }
        start += stride;
    }
    windows
}

pub fn extract_chunk(input: &Path, window: ChunkWindow) -> Result<PathBuf> {
    if window.duration() > DEFAULT_CHUNK_SECONDS + 0.001 {
        bail!(
            "planned transcription chunk exceeds {:.1}s ceiling: {:.3}s",
            DEFAULT_CHUNK_SECONDS,
            window.duration()
        );
    }

    let ffmpeg = find_ffmpeg_path().context("ffmpeg not found")?;
    let output_path = get_vibe_temp_folder().join(format!("chunk_{}.wav", random_string(12)));
    let start = format!("{:.3}", window.start);
    let duration = format!("{:.3}", window.duration().max(0.01));

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
/// Every overlap segment is preserved for conservative reconciliation later.
pub fn globalize_segments(segments: Vec<Segment>, window: ChunkWindow, media_duration: f64) -> Vec<Segment> {
    let offset = (window.start * 100.0).round() as i64;
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
/// Ambiguous variants and legitimate sequential repetition are preserved.
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
    fn plans_fixed_thirty_second_requests_with_two_second_overlap() {
        let windows = plan_chunks(95.0);
        assert_eq!(
            windows,
            vec![
                ChunkWindow { start: 0.0, end: 30.0 },
                ChunkWindow { start: 28.0, end: 58.0 },
                ChunkWindow { start: 56.0, end: 86.0 },
                ChunkWindow { start: 84.0, end: 95.0 },
            ]
        );
    }

    #[test]
    fn planned_windows_cover_without_gaps_and_never_exceed_ceiling() {
        for duration in [30.1, 31.0, 59.9, 60.0, 95.0, 300.0, 3600.0, 7200.0] {
            let windows = plan_chunks(duration);
            assert_eq!(windows.first().unwrap().start, 0.0);
            assert_eq!(windows.last().unwrap().end, duration);
            assert!(windows.iter().all(|window| window.duration() <= DEFAULT_CHUNK_SECONDS));
            for pair in windows.windows(2) {
                assert!(pair[1].start <= pair[0].end);
                let overlap = pair[0].end - pair[1].start;
                assert!(overlap >= 0.0);
                assert!(overlap <= DEFAULT_CHUNK_OVERLAP_SECONDS + 0.001);
            }
        }
    }

    #[test]
    fn globalizes_every_overlap_segment_without_dropping_it() {
        let window = ChunkWindow { start: 28.0, end: 58.0 };
        let local = vec![
            segment(0, 50, "left overlap"),
            segment(200, 300, "middle"),
            segment(2900, 3000, "right edge"),
        ];
        let global = globalize_segments(local, window, 90.0);
        assert_eq!(global.len(), 3);
        assert_eq!((global[0].start, global[0].stop), (2800, 2850));
        assert_eq!((global[1].start, global[1].stop), (3000, 3100));
        assert_eq!((global[2].start, global[2].stop), (5700, 5800));
    }

    #[test]
    fn merge_preserves_cross_boundary_phrase() {
        let left = globalize_segments(
            vec![segment(2800, 3000, "frase que cruza a fronteira")],
            ChunkWindow { start: 0.0, end: 30.0 },
            90.0,
        );
        let right = globalize_segments(
            vec![segment(0, 200, "frase que cruza a fronteira")],
            ChunkWindow { start: 28.0, end: 58.0 },
            90.0,
        );
        let merged = merge_segments(left.into_iter().chain(right).collect());
        assert_eq!(merged.len(), 1);
        assert_eq!((merged[0].start, merged[0].stop), (2800, 3000));
    }

    #[test]
    fn merge_preserves_ambiguous_boundary_variants() {
        let merged = merge_segments(vec![
            segment(2800, 3000, "vamos começar agora"),
            segment(2800, 2950, "começar agora"),
        ]);
        assert_eq!(merged.len(), 2);
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
