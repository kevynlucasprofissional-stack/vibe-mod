use crate::diagnostics::{input_file_metadata, DiagnosticsState};
use crate::error::LogError;
use crate::ffmpeg::get_vibe_temp_folder;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, Sample, SizedSample, Stream, SupportedStreamConfig};
use eyre::{bail, eyre, Context, ContextCompat, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::File;
use std::io::BufWriter;
use std::ops::Mul;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Listener, Manager};

use crate::ffmpeg::{get_local_time, random_string};

type WavWriterHandle = Arc<Mutex<Option<hound::WavWriter<BufWriter<File>>>>>;

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AudioDevice {
    pub is_default: bool,
    pub is_input: bool,
    pub id: String,
    pub name: String,
}

#[tauri::command]
pub fn get_audio_devices() -> Result<Vec<AudioDevice>> {
    let host = cpal::default_host();
    let mut audio_devices = Vec::new();

    let default_in = host
        .default_input_device()
        .map(|e| e.description().map(|d| d.to_string()))
        .context("name")?;
    let default_out = host
        .default_output_device()
        .map(|e| e.description().map(|d| d.to_string()))
        .context("name")?;
    tracing::debug!("Default Input Device:\n{:?}", default_in);
    tracing::debug!("Default Output Device:\n{:?}", default_out);

    let devices = host.devices()?;
    for (device_index, device) in devices.enumerate() {
        let name = device.description()?.to_string();
        let is_default_in = default_in.as_ref().is_ok_and(|d| d == &name);
        let is_default_out = default_out.as_ref().is_ok_and(|d| d == &name);
        audio_devices.push(AudioDevice {
            is_default: is_default_in || is_default_out,
            is_input: device.supports_input(),
            id: device_index.to_string(),
            name,
        });
    }
    Ok(audio_devices)
}

struct StreamHandle(Stream);
unsafe impl Send for StreamHandle {}
unsafe impl Sync for StreamHandle {}

#[tauri::command]
/// Record audio from the given devices, store to wav, merge with ffmpeg, and return path.
pub async fn start_record(
    app_handle: AppHandle,
    devices: Vec<AudioDevice>,
    store_in_documents: bool,
    custom_path: Option<String>,
    recording_name: Option<String>,
) -> Result<()> {
    let diagnostics = app_handle.state::<DiagnosticsState>();
    let run_id = diagnostics
        .start_run(
            "recording",
            json!({
                "devices": devices.iter().map(|device| json!({
                    "id": device.id,
                    "name": device.name,
                    "is_input": device.is_input,
                    "is_default": device.is_default,
                })).collect::<Vec<_>>(),
                "store_in_documents": store_in_documents,
                "custom_path": custom_path.clone(),
                "recording_name": recording_name.clone(),
            }),
        )
        .ok();

    let result = setup_recording(
        &app_handle,
        devices,
        store_in_documents,
        custom_path,
        recording_name,
        run_id.clone(),
    );

    if let Err(error) = &result {
        if let Some(id) = run_id.as_deref() {
            diagnostics
                .record_event(
                    id,
                    "error",
                    "recording",
                    "recording.setup_failed",
                    "Recording setup failed before capture started",
                    json!({ "error": format!("{error:#}") }),
                )
                .log_error();
            diagnostics
                .finish_run(id, "failed", json!({ "error": format!("{error:#}") }))
                .log_error();
        }
    }
    result
}

fn setup_recording(
    app_handle: &AppHandle,
    devices: Vec<AudioDevice>,
    store_in_documents: bool,
    custom_path: Option<String>,
    recording_name: Option<String>,
    run_id: Option<String>,
) -> Result<()> {
    let host = cpal::default_host();
    let diagnostics = app_handle.state::<DiagnosticsState>();
    if devices.is_empty() {
        diag(
            &diagnostics,
            run_id.as_deref(),
            "warning",
            "recording.no_devices",
            "Recording was requested without any selected audio devices",
            json!({}),
        );
    }

    let mut wav_paths: Vec<(PathBuf, u32)> = Vec::new();
    let mut stream_handles = Vec::new();
    let mut stream_writers = Vec::new();

    for selected in devices {
        diag(
            &diagnostics,
            run_id.as_deref(),
            "info",
            "recording.device_open_started",
            "Opening selected audio device",
            json!({ "id": selected.id, "name": selected.name, "is_input": selected.is_input }),
        );

        let is_input = selected.is_input;
        let selected_name = selected.name.clone();
        let (device, config) = if is_input {
            let device_id: usize = selected.id.parse().context("Failed to parse device ID")?;
            let dev = host.devices()?.nth(device_id).context("Failed to get device by ID")?;
            let config = dev.default_input_config().context("Failed to get default input config")?;
            (dev, config)
        } else {
            get_output_device_and_config(&host, &selected)?
        };
        let spec = wav_spec_from_config(&config);
        let config_data = json!({
            "device": selected_name,
            "sample_rate": config.sample_rate(),
            "channels": config.channels(),
            "sample_format": format!("{:?}", config.sample_format()),
        });

        let path = get_vibe_temp_folder().join(format!("{}.wav", random_string(10)));
        wav_paths.push((path.clone(), 0));
        let writer = hound::WavWriter::create(path.clone(), spec)?;
        let writer = Arc::new(Mutex::new(Some(writer)));
        stream_writers.push(writer.clone());

        let stream = build_input_stream(&device, config, writer.clone())?;
        stream.play()?;
        stream_handles.push(Arc::new(Mutex::new(Some(StreamHandle(stream)))));
        diag(
            &diagnostics,
            run_id.as_deref(),
            "info",
            "recording.device_open_completed",
            "Audio capture stream started",
            config_data,
        );
    }

    let app_handle_clone = app_handle.clone();
    app_handle.once("stop_record", move |_event| {
        let diagnostics = app_handle_clone.state::<DiagnosticsState>();
        diag(
            &diagnostics,
            run_id.as_deref(),
            "info",
            "recording.stop_requested",
            "Recording stop event received",
            json!({}),
        );
        let mut warnings = Vec::<Value>::new();

        for (index, stream_handle) in stream_handles.iter().enumerate() {
            let stream_handle = stream_handle.lock().map_err(|e| eyre!("{:?}", e)).log_error();
            if let Some(mut stream_handle) = stream_handle {
                if let Some(stream) = stream_handle.take() {
                    if let Err(error) = stream.0.pause() {
                        warnings.push(json!({ "stage": "pause", "index": index, "error": error.to_string() }));
                    }
                    let writer = stream_writers[index].clone();
                    let writer = writer.lock().expect("lock").take();
                    if let Some(writer) = writer {
                        let written = writer.len();
                        wav_paths[index] = (wav_paths[index].0.clone(), written);
                        if let Err(error) = writer.finalize() {
                            warnings.push(json!({ "stage": "finalize_wav", "index": index, "error": error.to_string() }));
                        }
                    }
                }
            }
        }

        let samples: Vec<Value> = wav_paths
            .iter()
            .enumerate()
            .map(|(index, (path, count))| json!({ "index": index, "samples": count, "file": input_file_metadata(path) }))
            .collect();
        diag(
            &diagnostics,
            run_id.as_deref(),
            "info",
            "recording.capture_finalized",
            "Capture writers were finalized",
            json!({ "sources": samples }),
        );

        let source = choose_or_merge_recording_sources(&wav_paths, &diagnostics, run_id.as_deref(), &mut warnings);
        let Some(source) = source else {
            if let Some(id) = run_id.as_deref() {
                diagnostics
                    .finish_run(id, "failed", json!({ "error": "No usable recording source was produced", "warnings": warnings }))
                    .log_error();
            }
            cleanup_wavs(&wav_paths, &diagnostics, run_id.as_deref());
            return;
        };

        let recording_stem = recording_name
            .as_deref()
            .map(crate::cmd::files::sanitize_filename_stem)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(get_local_time);
        let temp_dir = get_vibe_temp_folder();
        let mut normalized = crate::cmd::files::available_path(&temp_dir, &recording_stem, "wav");

        diag(
            &diagnostics,
            run_id.as_deref(),
            "info",
            "recording.normalize_started",
            "Normalizing captured audio with FFmpeg",
            json!({ "source": input_file_metadata(&source), "target": normalized.clone() }),
        );
        match crate::ffmpeg::normalize(source.clone(), normalized.clone(), None) {
            Ok(()) if normalized.exists() => {
                diag(
                    &diagnostics,
                    run_id.as_deref(),
                    "info",
                    "recording.normalize_completed",
                    "Audio normalization completed",
                    json!({ "output": input_file_metadata(&normalized) }),
                );
            }
            Ok(()) => {
                warnings.push(json!({ "stage": "normalize", "error": "FFmpeg returned success but normalized output is missing" }));
                normalized = source.clone();
            }
            Err(error) => {
                warnings.push(json!({ "stage": "normalize", "error": format!("{error:#}") }));
                diag(
                    &diagnostics,
                    run_id.as_deref(),
                    "warning",
                    "recording.normalize_failed",
                    "Normalization failed; preserving the original captured WAV as fallback",
                    json!({ "error": format!("{error:#}"), "fallback": input_file_metadata(&source) }),
                );
                normalized = source.clone();
            }
        }

        if store_in_documents {
            let save_dir = custom_path
                .as_ref()
                .map(PathBuf::from)
                .or_else(|| {
                    app_handle_clone
                        .path()
                        .document_dir()
                        .ok()
                        .map(|dir| dir.join(crate::config::DOCUMENTS_SUBFOLDER))
                });
            if let Some(save_dir) = save_dir {
                match std::fs::create_dir_all(&save_dir) {
                    Ok(()) => {
                        let target_path = crate::cmd::files::available_path(&save_dir, &recording_stem, "wav");
                        match std::fs::rename(&normalized, &target_path) {
                            Ok(()) => {
                                diag(
                                    &diagnostics,
                                    run_id.as_deref(),
                                    "info",
                                    "recording.persist_moved",
                                    "Recording moved to final storage",
                                    json!({ "target": target_path.clone() }),
                                );
                                normalized = target_path;
                            }
                            Err(rename_error) => match std::fs::copy(&normalized, &target_path) {
                                Ok(_) => {
                                    if let Err(remove_error) = std::fs::remove_file(&normalized) {
                                        warnings.push(json!({ "stage": "remove_after_copy", "error": remove_error.to_string() }));
                                    }
                                    diag(
                                        &diagnostics,
                                        run_id.as_deref(),
                                        "warning",
                                        "recording.persist_copied",
                                        "Move failed across filesystems; recording was copied instead",
                                        json!({ "rename_error": rename_error.to_string(), "target": target_path.clone() }),
                                    );
                                    normalized = target_path;
                                }
                                Err(copy_error) => {
                                    warnings.push(json!({
                                        "stage": "persist",
                                        "rename_error": rename_error.to_string(),
                                        "copy_error": copy_error.to_string(),
                                    }));
                                }
                            },
                        }
                    }
                    Err(error) => warnings.push(json!({ "stage": "create_recording_directory", "error": error.to_string() })),
                }
            } else {
                warnings.push(json!({ "stage": "resolve_recording_directory", "error": "No documents/custom directory available" }));
            }
        }

        cleanup_wavs_except(&wav_paths, &normalized, &diagnostics, run_id.as_deref());
        if source != normalized && source.exists() {
            if let Err(error) = std::fs::remove_file(&source) {
                warnings.push(json!({ "stage": "cleanup_source", "error": error.to_string() }));
            }
        }

        let final_exists = normalized.exists();
        let final_file = input_file_metadata(&normalized);
        if final_exists {
            app_handle_clone
                .emit(
                    "record_finish",
                    json!({
                        "path": normalized.to_string_lossy(),
                        "name": normalized.file_name().and_then(|name| name.to_str()).unwrap_or_default(),
                    }),
                )
                .map_err(|e| eyre!("{e:?}"))
                .log_error();
        } else {
            warnings.push(json!({ "stage": "final_validation", "error": "Final recording path does not exist" }));
        }

        if let Some(id) = run_id.as_deref() {
            let outcome = if !final_exists { "failed" } else if warnings.is_empty() { "succeeded" } else { "partial" };
            diagnostics
                .finish_run(
                    id,
                    outcome,
                    json!({
                        "final_file": final_file,
                        "warnings": warnings,
                        "store_in_documents": store_in_documents,
                    }),
                )
                .log_error();
        }
    });

    Ok(())
}

fn choose_or_merge_recording_sources(
    wav_paths: &[(PathBuf, u32)],
    diagnostics: &DiagnosticsState,
    run_id: Option<&str>,
    warnings: &mut Vec<Value>,
) -> Option<PathBuf> {
    match wav_paths {
        [] => None,
        [(path, _)] => {
            diag(diagnostics, run_id, "info", "recording.source_single", "Using the single captured WAV source", json!({ "source": path }));
            Some(path.clone())
        }
        paths => {
            let first = &paths[0];
            let second = &paths[1];
            if first.1 > 0 && second.1 > 0 {
                let dst = get_vibe_temp_folder().join(format!("{}.wav", random_string(10)));
                match crate::ffmpeg::merge_wav_files(first.0.clone(), second.0.clone(), dst.clone()) {
                    Ok(()) if dst.exists() => {
                        diag(
                            diagnostics,
                            run_id,
                            "info",
                            "recording.sources_merged",
                            "Input and system-audio sources were merged",
                            json!({ "output": input_file_metadata(&dst) }),
                        );
                        return Some(dst);
                    }
                    Ok(()) => warnings.push(json!({ "stage": "merge", "error": "FFmpeg returned success but merge output is missing" })),
                    Err(error) => warnings.push(json!({ "stage": "merge", "error": format!("{error:#}") })),
                }
            }
            let fallback = if first.1 >= second.1 { &first.0 } else { &second.0 };
            diag(
                diagnostics,
                run_id,
                "warning",
                "recording.merge_fallback",
                "Using the captured source with the larger sample count instead of a merged file",
                json!({ "selected": input_file_metadata(fallback), "first_samples": first.1, "second_samples": second.1 }),
            );
            Some(fallback.clone())
        }
    }
}

fn cleanup_wavs(paths: &[(PathBuf, u32)], diagnostics: &DiagnosticsState, run_id: Option<&str>) {
    cleanup_wavs_except(paths, &PathBuf::new(), diagnostics, run_id);
}

fn cleanup_wavs_except(paths: &[(PathBuf, u32)], keep: &PathBuf, diagnostics: &DiagnosticsState, run_id: Option<&str>) {
    for (path, _) in paths {
        if path == keep || !path.exists() {
            continue;
        }
        if let Err(error) = std::fs::remove_file(path) {
            diag(
                diagnostics,
                run_id,
                "warning",
                "recording.cleanup_failed",
                "Temporary recording file could not be removed",
                json!({ "path": path, "error": error.to_string() }),
            );
        }
    }
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
            .record_event(id, severity, "recording", stage, message, data)
            .log_error();
    }
}

#[allow(unused_variables)]
fn get_output_device_and_config(host: &cpal::Host, audio_device: &AudioDevice) -> Result<(Device, SupportedStreamConfig)> {
    #[cfg(target_os = "macos")]
    {
        let device = host.default_output_device().context("Failed to get default output device")?;
        let config = device.default_output_config().context("Failed to get default output config")?;
        Ok((device, config))
    }

    #[cfg(not(target_os = "macos"))]
    {
        let device_id: usize = audio_device.id.parse().context("Failed to parse device ID")?;
        let device = host.devices()?.nth(device_id).context("Failed to get device by ID")?;
        let config = device.default_output_config().context("Failed to get default output config")?;
        Ok((device, config))
    }
}

fn build_input_stream_typed<T>(device: &Device, config: SupportedStreamConfig, writer: WavWriterHandle) -> Result<Stream>
where
    T: SizedSample + hound::Sample + FromSample<T> + Mul<Output = T> + Copy,
{
    let stream = device.build_input_stream(
        config.into(),
        move |data: &[T], _: &_| write_input_data::<T, T>(data, &writer),
        |err| tracing::error!("An error occurred on stream: {}", err),
        None,
    )?;
    Ok(stream)
}

fn build_input_stream(device: &Device, config: SupportedStreamConfig, writer: WavWriterHandle) -> Result<Stream> {
    match config.sample_format() {
        cpal::SampleFormat::I8 => build_input_stream_typed::<i8>(device, config, writer),
        cpal::SampleFormat::I16 => build_input_stream_typed::<i16>(device, config, writer),
        cpal::SampleFormat::I32 => build_input_stream_typed::<i32>(device, config, writer),
        cpal::SampleFormat::F32 => build_input_stream_typed::<f32>(device, config, writer),
        sample_format => bail!("Unsupported sample format '{}'", sample_format),
    }
}

fn sample_format(format: cpal::SampleFormat) -> hound::SampleFormat {
    if format.is_float() {
        hound::SampleFormat::Float
    } else {
        hound::SampleFormat::Int
    }
}

fn wav_spec_from_config(config: &cpal::SupportedStreamConfig) -> hound::WavSpec {
    hound::WavSpec {
        channels: config.channels() as _,
        sample_rate: config.sample_rate(),
        bits_per_sample: (config.sample_format().sample_size() * 8) as _,
        sample_format: sample_format(config.sample_format()),
    }
}

fn write_input_data<T, U>(input: &[T], writer: &WavWriterHandle)
where
    T: Sample,
    U: Sample + hound::Sample + FromSample<T> + Mul<Output = U> + Copy,
{
    if let Ok(mut guard) = writer.try_lock() {
        if let Some(writer) = guard.as_mut() {
            for &sample in input.iter() {
                let sample: U = U::from_sample(sample);
                writer.write_sample(sample).ok();
            }
        }
    }
}
