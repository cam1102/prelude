use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const AUDIO_RUNTIME_STATUS_PATH: &str = "/run/prelude/audio-runtime.json";
pub const AUDIO_RUNTIME_STATUS_VERSION: u8 = 8;
const MAXIMUM_STATUS_BYTES: u64 = 16 * 1024;
static NEXT_STATUS_TEMPORARY: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioRuntimeRoute {
    Disabled,
    UsbDac,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioPipelineState {
    Disabled,
    PreflightReady,
    WaitingForStream,
    Playing,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioFailureKind {
    Endpoint,
    IngressBackpressure,
    Pipeline,
    Bus,
    MediaWorker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioBaseSinkStatus {
    pub rendered_buffers: u64,
    pub dropped_buffers: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioBackendStatus {
    pub pipeline_start_events: u64,
    pub pipeline_stop_events: u64,
    pub frames_pushed: u64,
    pub frame_push_failure_events: u64,
    pub bus_poll_events: u64,
    pub bus_failure_events: u64,
    pub endpoint_revalidation_events: u64,
    pub endpoint_revalidation_failure_events: u64,
    pub sink_stats_poll_events: u64,
    pub base_sink: Option<AudioBaseSinkStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MicrophoneRuntimeStatus {
    pub negotiated: bool,
    pub capture_running: bool,
    pub ready: bool,
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub send_failure_events: u64,
    pub rms_q15: Option<u16>,
    pub peak_q15: Option<u16>,
}

impl MicrophoneRuntimeStatus {
    pub const fn inactive() -> Self {
        Self {
            negotiated: false,
            capture_running: false,
            ready: false,
            packets_sent: 0,
            bytes_sent: 0,
            send_failure_events: 0,
            rms_q15: None,
            peak_q15: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioLatencyStatus {
    pub default_opus_jitter_ms: u32,
    pub default_aac_jitter_ms: u32,
    pub sink_buffer_micros: u32,
    pub sink_period_micros: u32,
    pub gstreamer_query_live: Option<bool>,
    pub gstreamer_query_minimum_ns: Option<u64>,
    pub gstreamer_query_maximum_ns: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioStreamLatencyStatus {
    pub stream_type: u16,
    pub effective_playout_latency_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioOutputDelaySourceStatus {
    Uncompensated,
    ConfiguredCalibration,
    TransportReported,
    Measured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VideoSynchronizationStatus {
    pub output_delay_source: AudioOutputDelaySourceStatus,
    pub output_delay_ms: u32,
    pub presented_frames: u64,
    pub presentation_deadline_miss_events: u64,
    pub total_scheduled_wait_ns: u64,
    pub maximum_presentation_lateness_ns: u64,
}

impl VideoSynchronizationStatus {
    pub const fn uncompensated() -> Self {
        Self {
            output_delay_source: AudioOutputDelaySourceStatus::Uncompensated,
            output_delay_ms: 0,
            presented_frames: 0,
            presentation_deadline_miss_events: 0,
            total_scheduled_wait_ns: 0,
            maximum_presentation_lateness_ns: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioRuntimeSnapshot {
    pub version: u8,
    pub requested_route: AudioRuntimeRoute,
    pub active_route: Option<AudioRuntimeRoute>,
    pub pipeline_state: AudioPipelineState,
    pub pipeline_generation: u64,
    pub recording: bool,
    pub negotiated_streams: usize,
    pub muted: bool,
    pub ducked: bool,
    pub frames_accepted: u64,
    pub ingress_full_events: u64,
    pub pipeline_failure_events: u64,
    pub main_pcm_level_observations: u64,
    pub main_pcm_mean_abs_q15: Option<u16>,
    pub main_pcm_peak_q15: Option<u16>,
    pub main_pcm_be_mean_channel_delta: Option<u16>,
    pub main_pcm_le_mean_abs_q15: Option<u16>,
    pub main_pcm_le_peak_q15: Option<u16>,
    pub main_pcm_le_mean_channel_delta: Option<u16>,
    pub backend: Option<AudioBackendStatus>,
    pub microphone: MicrophoneRuntimeStatus,
    pub latency: Option<AudioLatencyStatus>,
    pub stream_latencies: Vec<AudioStreamLatencyStatus>,
    pub video_synchronization: VideoSynchronizationStatus,
    pub last_failure: Option<AudioFailureKind>,
}

impl AudioRuntimeSnapshot {
    pub const fn disabled() -> Self {
        Self {
            version: AUDIO_RUNTIME_STATUS_VERSION,
            requested_route: AudioRuntimeRoute::Disabled,
            active_route: None,
            pipeline_state: AudioPipelineState::Disabled,
            pipeline_generation: 0,
            recording: false,
            negotiated_streams: 0,
            muted: false,
            ducked: false,
            frames_accepted: 0,
            ingress_full_events: 0,
            pipeline_failure_events: 0,
            main_pcm_level_observations: 0,
            main_pcm_mean_abs_q15: None,
            main_pcm_peak_q15: None,
            main_pcm_be_mean_channel_delta: None,
            main_pcm_le_mean_abs_q15: None,
            main_pcm_le_peak_q15: None,
            main_pcm_le_mean_channel_delta: None,
            backend: None,
            microphone: MicrophoneRuntimeStatus::inactive(),
            latency: None,
            stream_latencies: Vec::new(),
            video_synchronization: VideoSynchronizationStatus::uncompensated(),
            last_failure: None,
        }
    }

    pub fn waiting(requested_route: AudioRuntimeRoute) -> Self {
        Self {
            requested_route,
            pipeline_state: AudioPipelineState::WaitingForStream,
            ..Self::disabled()
        }
    }

    pub fn validate(&self) -> Result<(), AudioStatusError> {
        if self.version != AUDIO_RUNTIME_STATUS_VERSION
            || self.negotiated_streams > 3
            || self.stream_latencies.len() > 3
            || self.video_synchronization.output_delay_ms
                > prelude_media::MAXIMUM_AUDIO_OUTPUT_DELAY_MS
            || matches!(
                self.video_synchronization.output_delay_source,
                AudioOutputDelaySourceStatus::Uncompensated
            ) && self.video_synchronization.output_delay_ms != 0
            || matches!(self.requested_route, AudioRuntimeRoute::UsbDac)
                && !matches!(
                    self.video_synchronization.output_delay_source,
                    AudioOutputDelaySourceStatus::Uncompensated
                )
            || self.video_synchronization.presentation_deadline_miss_events
                > self.video_synchronization.presented_frames
            || matches!(self.requested_route, AudioRuntimeRoute::Disabled)
                && (self.active_route.is_some()
                    || !matches!(self.pipeline_state, AudioPipelineState::Disabled))
            || self.active_route.is_some()
                && !matches!(self.pipeline_state, AudioPipelineState::Playing)
            || matches!(self.pipeline_state, AudioPipelineState::Playing)
                && self.active_route != Some(self.requested_route)
            || self.backend.is_some()
                && !matches!(
                    self.pipeline_state,
                    AudioPipelineState::Playing | AudioPipelineState::Failed
                )
            || self.backend.is_some_and(|backend| {
                backend.base_sink.is_some() && backend.sink_stats_poll_events == 0
            })
            || self.microphone.capture_running && !self.microphone.negotiated
            || self.microphone.ready
                && (!self.microphone.capture_running || self.microphone.packets_sent == 0)
            || self
                .microphone
                .rms_q15
                .is_some_and(|level| level > i16::MAX as u16)
            || self
                .microphone
                .peak_q15
                .is_some_and(|level| level > i16::MAX as u16)
            || matches!(
                (self.microphone.rms_q15, self.microphone.peak_q15),
                (Some(rms), Some(peak)) if rms > peak
            )
            || (self.main_pcm_level_observations == 0
                && (self.main_pcm_mean_abs_q15.is_some()
                    || self.main_pcm_peak_q15.is_some()
                    || self.main_pcm_be_mean_channel_delta.is_some()
                    || self.main_pcm_le_mean_abs_q15.is_some()
                    || self.main_pcm_le_peak_q15.is_some()
                    || self.main_pcm_le_mean_channel_delta.is_some()))
            || (self.main_pcm_level_observations != 0
                && (self.main_pcm_mean_abs_q15.is_none()
                    || self.main_pcm_peak_q15.is_none()
                    || self.main_pcm_be_mean_channel_delta.is_none()
                    || self.main_pcm_le_mean_abs_q15.is_none()
                    || self.main_pcm_le_peak_q15.is_none()
                    || self.main_pcm_le_mean_channel_delta.is_none()))
            || self
                .main_pcm_mean_abs_q15
                .is_some_and(|level| level > 32_768)
            || self.main_pcm_peak_q15.is_some_and(|level| level > 32_768)
            || self
                .main_pcm_le_mean_abs_q15
                .is_some_and(|level| level > 32_768)
            || self
                .main_pcm_le_peak_q15
                .is_some_and(|level| level > 32_768)
            || matches!(
                (self.main_pcm_mean_abs_q15, self.main_pcm_peak_q15),
                (Some(mean), Some(peak)) if mean > peak
            )
            || matches!(
                (self.main_pcm_le_mean_abs_q15, self.main_pcm_le_peak_q15),
                (Some(mean), Some(peak)) if mean > peak
            )
        {
            return Err(AudioStatusError::Invalid);
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct AudioStatusWriter {
    path: PathBuf,
}

impl AudioStatusWriter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write(&self, snapshot: &AudioRuntimeSnapshot) -> Result<(), AudioStatusError> {
        snapshot.validate()?;
        let directory = self.path.parent().ok_or(AudioStatusError::UnsafePath)?;
        fs::create_dir_all(directory).map_err(AudioStatusError::Io)?;
        let mut encoded = serde_json::to_vec_pretty(snapshot).map_err(AudioStatusError::Json)?;
        encoded.push(b'\n');
        if encoded.len() as u64 > MAXIMUM_STATUS_BYTES {
            return Err(AudioStatusError::Oversize);
        }
        let (mut file, temporary) = create_status_temporary(directory)?;
        let result = (|| -> std::io::Result<()> {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(&encoded)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            fs::File::open(directory)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(AudioStatusError::Io)
    }
}

fn create_status_temporary(directory: &Path) -> Result<(fs::File, PathBuf), AudioStatusError> {
    let first_sequence = NEXT_STATUS_TEMPORARY.fetch_add(32, Ordering::Relaxed);
    create_status_temporary_from(directory, first_sequence)
}

fn create_status_temporary_from(
    directory: &Path,
    first_sequence: u64,
) -> Result<(fs::File, PathBuf), AudioStatusError> {
    for offset in 0..32_u64 {
        let sequence = first_sequence.wrapping_add(offset);
        let path = directory.join(format!(
            ".audio-runtime.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(AudioStatusError::Io(error)),
        }
    }
    Err(AudioStatusError::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique audio status temporary file",
    )))
}

pub fn load_audio_runtime_status(
    path: &Path,
) -> Result<Option<AudioRuntimeSnapshot>, AudioStatusError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AudioStatusError::Io(error)),
    };
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAXIMUM_STATUS_BYTES
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(AudioStatusError::UnsafeFile);
    }
    let mut contents = Vec::new();
    fs::File::open(path)
        .map_err(AudioStatusError::Io)?
        .take(MAXIMUM_STATUS_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(AudioStatusError::Io)?;
    if contents.len() as u64 > MAXIMUM_STATUS_BYTES {
        return Err(AudioStatusError::Oversize);
    }
    let snapshot: AudioRuntimeSnapshot =
        serde_json::from_slice(&contents).map_err(AudioStatusError::Json)?;
    snapshot.validate()?;
    Ok(Some(snapshot))
}

#[derive(Debug, Error)]
pub enum AudioStatusError {
    #[error("audio runtime status I/O failed: {0}")]
    Io(#[source] std::io::Error),
    #[error("audio runtime status JSON is invalid: {0}")]
    Json(#[source] serde_json::Error),
    #[error("audio runtime status path is unsafe")]
    UnsafePath,
    #[error("audio runtime status file type, size, or permissions are unsafe")]
    UnsafeFile,
    #[error("audio runtime status exceeds its size bound")]
    Oversize,
    #[error("audio runtime status contains an impossible state")]
    Invalid,
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::TempDir;

    use super::*;

    fn active_snapshot() -> AudioRuntimeSnapshot {
        AudioRuntimeSnapshot {
            requested_route: AudioRuntimeRoute::UsbDac,
            active_route: Some(AudioRuntimeRoute::UsbDac),
            pipeline_state: AudioPipelineState::Playing,
            pipeline_generation: 2,
            recording: true,
            negotiated_streams: 2,
            frames_accepted: 42,
            main_pcm_level_observations: 2,
            main_pcm_mean_abs_q15: Some(2_400),
            main_pcm_peak_q15: Some(12_000),
            main_pcm_be_mean_channel_delta: Some(800),
            main_pcm_le_mean_abs_q15: Some(15_900),
            main_pcm_le_peak_q15: Some(32_700),
            main_pcm_le_mean_channel_delta: Some(18_000),
            backend: Some(AudioBackendStatus {
                pipeline_start_events: 1,
                pipeline_stop_events: 0,
                frames_pushed: 40,
                frame_push_failure_events: 0,
                bus_poll_events: 400,
                bus_failure_events: 0,
                endpoint_revalidation_events: 2,
                endpoint_revalidation_failure_events: 0,
                sink_stats_poll_events: 2,
                base_sink: Some(AudioBaseSinkStatus {
                    rendered_buffers: 40,
                    dropped_buffers: 0,
                }),
            }),
            latency: Some(AudioLatencyStatus {
                default_opus_jitter_ms: 100,
                default_aac_jitter_ms: 1_000,
                sink_buffer_micros: 100_000,
                sink_period_micros: 20_000,
                gstreamer_query_live: Some(true),
                gstreamer_query_minimum_ns: Some(20_000_000),
                gstreamer_query_maximum_ns: None,
            }),
            stream_latencies: vec![AudioStreamLatencyStatus {
                stream_type: 102,
                effective_playout_latency_ms: 730,
            }],
            video_synchronization: VideoSynchronizationStatus {
                output_delay_source: AudioOutputDelaySourceStatus::Uncompensated,
                output_delay_ms: 0,
                presented_frames: 80,
                presentation_deadline_miss_events: 1,
                total_scheduled_wait_ns: 0,
                maximum_presentation_lateness_ns: 40_000_000,
            },
            ..AudioRuntimeSnapshot::disabled()
        }
    }

    #[test]
    fn status_is_private_atomic_bounded_and_redacted() {
        let directory = TempDir::new().expect("tempdir");
        let path = directory.path().join("audio-runtime.json");
        let writer = AudioStatusWriter::new(&path);
        writer.write(&active_snapshot()).expect("first write");
        writer.write(&active_snapshot()).expect("atomic replace");
        assert_eq!(
            load_audio_runtime_status(&path).expect("load"),
            Some(active_snapshot())
        );
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
        let text = fs::read_to_string(&path).expect("status");
        for private in ["CARD=", "30:E3:", "serial", "vehicle"] {
            assert!(!text.contains(private));
        }
        for misleading in ["alsa_xrun", "underrun", "device", "address"] {
            assert!(!text.contains(misleading));
        }
        assert_eq!(fs::read_dir(directory.path()).expect("entries").count(), 1);
    }

    #[test]
    fn exact_stale_temporary_collision_does_not_block_checkpoint() {
        let directory = TempDir::new().expect("tempdir");
        let path = directory.path().join("audio-runtime.json");
        let first_sequence = 18_446;
        let stale = directory.path().join(format!(
            ".audio-runtime.{}.{}.tmp",
            std::process::id(),
            first_sequence
        ));
        fs::write(&stale, b"incomplete").expect("stale temporary");
        let (temporary_file, temporary_path) =
            create_status_temporary_from(directory.path(), first_sequence)
                .expect("skip exact stale collision");
        drop(temporary_file);
        assert_ne!(temporary_path, stale);
        fs::remove_file(&temporary_path).expect("remove test temporary");

        let writer = AudioStatusWriter::new(&path);
        writer.write(&active_snapshot()).expect("checkpoint");
        assert_eq!(
            load_audio_runtime_status(&path).expect("load"),
            Some(active_snapshot())
        );
        assert_eq!(fs::read(&stale).expect("stale remains"), b"incomplete");
    }

    #[test]
    fn telemetry_schema_round_trips_saturated_counters() {
        let directory = TempDir::new().expect("tempdir");
        let path = directory.path().join("audio-runtime.json");
        let mut snapshot = active_snapshot();
        let backend = snapshot.backend.as_mut().expect("backend telemetry");
        backend.pipeline_start_events = u64::MAX;
        backend.pipeline_stop_events = u64::MAX;
        backend.frames_pushed = u64::MAX;
        backend.frame_push_failure_events = u64::MAX;
        backend.bus_poll_events = u64::MAX;
        backend.bus_failure_events = u64::MAX;
        backend.endpoint_revalidation_events = u64::MAX;
        backend.endpoint_revalidation_failure_events = u64::MAX;
        backend.sink_stats_poll_events = u64::MAX;
        backend.base_sink = Some(AudioBaseSinkStatus {
            rendered_buffers: u64::MAX,
            dropped_buffers: u64::MAX,
        });
        snapshot.microphone = MicrophoneRuntimeStatus {
            negotiated: true,
            capture_running: true,
            ready: true,
            packets_sent: u64::MAX,
            bytes_sent: u64::MAX,
            send_failure_events: u64::MAX,
            rms_q15: Some(12_345),
            peak_q15: Some(i16::MAX as u16),
        };
        snapshot.video_synchronization = VideoSynchronizationStatus {
            output_delay_source: AudioOutputDelaySourceStatus::Uncompensated,
            output_delay_ms: 0,
            presented_frames: u64::MAX,
            presentation_deadline_miss_events: u64::MAX,
            total_scheduled_wait_ns: u64::MAX,
            maximum_presentation_lateness_ns: u64::MAX,
        };
        AudioStatusWriter::new(&path)
            .write(&snapshot)
            .expect("write saturated status");
        assert_eq!(
            load_audio_runtime_status(&path).expect("read saturated status"),
            Some(snapshot)
        );
    }

    #[test]
    fn unknown_impossible_oversize_and_symlink_status_fail_closed() {
        let directory = TempDir::new().expect("tempdir");
        let path = directory.path().join("audio-runtime.json");
        fs::write(&path, br#"{"version":1,"unknown":true}"#).expect("unknown");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
        assert!(matches!(
            load_audio_runtime_status(&path),
            Err(AudioStatusError::Json(_))
        ));

        let mut impossible_microphone = active_snapshot();
        impossible_microphone.microphone.ready = true;
        assert!(matches!(
            impossible_microphone.validate(),
            Err(AudioStatusError::Invalid)
        ));
        let mut impossible_playback_level = active_snapshot();
        impossible_playback_level.main_pcm_level_observations = 0;
        assert!(matches!(
            impossible_playback_level.validate(),
            Err(AudioStatusError::Invalid)
        ));
        impossible_playback_level.main_pcm_level_observations = 1;
        impossible_playback_level.main_pcm_mean_abs_q15 = Some(12_001);
        assert!(matches!(
            impossible_playback_level.validate(),
            Err(AudioStatusError::Invalid)
        ));
        fs::write(&path, vec![b'x'; MAXIMUM_STATUS_BYTES as usize + 1]).expect("oversize");
        assert!(matches!(
            load_audio_runtime_status(&path),
            Err(AudioStatusError::UnsafeFile)
        ));
        fs::remove_file(&path).expect("remove");
        let target = directory.path().join("target");
        fs::write(&target, b"{}").expect("target");
        symlink(&target, &path).expect("symlink");
        assert!(matches!(
            load_audio_runtime_status(&path),
            Err(AudioStatusError::UnsafeFile)
        ));
    }

    #[test]
    fn output_delay_telemetry_rejects_false_or_unbounded_evidence() {
        let mut impossible = active_snapshot();
        impossible.video_synchronization.output_delay_source =
            AudioOutputDelaySourceStatus::Uncompensated;
        impossible.video_synchronization.output_delay_ms = 1;
        assert!(impossible.validate().is_err());

        let mut unbounded = active_snapshot();
        unbounded.video_synchronization.output_delay_ms =
            prelude_media::MAXIMUM_AUDIO_OUTPUT_DELAY_MS + 1;
        assert!(unbounded.validate().is_err());

        let mut compensated_usb = active_snapshot();
        compensated_usb.video_synchronization.output_delay_source =
            AudioOutputDelaySourceStatus::ConfiguredCalibration;
        compensated_usb.video_synchronization.output_delay_ms = 250;
        assert!(compensated_usb.validate().is_err());

        let mut impossible_counters = active_snapshot();
        impossible_counters
            .video_synchronization
            .presentation_deadline_miss_events = 81;
        assert!(impossible_counters.validate().is_err());
    }
}
