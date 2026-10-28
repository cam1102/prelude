use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

use prelude_wireless::access_point::{ChannelWidth, is_supported_channel};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{CarPlayDock, DaemonConfig, PRESET_COUNT, PresetAction};

const SETTINGS_FILE: &str = "settings.toml";
const MAXIMUM_SETTINGS_BYTES: u64 = 2048;

fn default_on() -> bool {
    true
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("reading settings failed: {0}")]
    Read(#[source] std::io::Error),
    #[error("writing settings failed: {0}")]
    Write(#[source] std::io::Error),
    #[error("decoding settings failed: {0}")]
    Decode(#[source] toml::de::Error),
    #[error("encoding settings failed: {0}")]
    Encode(#[source] toml::ser::Error),
    #[error("reported DPI is outside the Android Auto range {minimum}..={maximum}")]
    InvalidDpi { minimum: u32, maximum: u32 },
    #[error("Wi-Fi channel {0} is not a supported 2.4 GHz or non-DFS 5 GHz channel")]
    InvalidWifiChannel(u8),
    #[error("Wi-Fi channel width {0} MHz is not 20, 40, or 80")]
    InvalidWifiWidth(u16),
    #[error("invalid equalizer settings: {0}")]
    InvalidEqualizer(#[source] prelude_media::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeSettings {
    pub auto_brightness: bool,
    pub auto_night_mode: bool,
    pub reported_dpi: u32,
    #[serde(default = "default_original_dpi")]
    pub original_dpi: u32,
    pub dpi_customized: bool,
    pub phone_audio: bool,
    pub echo_cancellation: bool,
    #[serde(default = "default_on")]
    pub noise_suppression: bool,
    pub noise_suppression_level: prelude_media::NoiseSuppressionLevel,
    #[serde(default = "default_on")]
    pub high_pass_filter: bool,
    pub gain_control: bool,
    pub carplay_dock: CarPlayDock,
    pub presets: [PresetAction; PRESET_COUNT],
    pub equalizer: prelude_media::EqualizerSettings,
    pub wifi_channel: Option<u8>,
    pub wifi_channel_width_mhz: Option<u16>,
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            auto_brightness: true,
            auto_night_mode: true,
            reported_dpi: prelude_android_auto_protocol::DEFAULT_VIDEO_DENSITY_DPI,
            original_dpi: prelude_android_auto_protocol::DEFAULT_VIDEO_DENSITY_DPI,
            dpi_customized: false,
            phone_audio: false,
            echo_cancellation: false,
            noise_suppression: true,
            noise_suppression_level: prelude_media::NoiseSuppressionLevel::Moderate,
            high_pass_filter: true,
            gain_control: false,
            carplay_dock: CarPlayDock::Left,
            presets: PresetAction::DEFAULTS,
            equalizer: Default::default(),
            wifi_channel: None,
            wifi_channel_width_mhz: None,
        }
    }
}

fn default_original_dpi() -> u32 {
    prelude_android_auto_protocol::DEFAULT_VIDEO_DENSITY_DPI
}

impl RuntimeSettings {
    pub fn from_config(config: &DaemonConfig) -> Self {
        Self {
            auto_brightness: config.platform.ambient_light.automatic_brightness,
            auto_night_mode: config.platform.ambient_light.automatic_night_mode,
            reported_dpi: config.android_auto.ui_density_dpi,
            original_dpi: config.android_auto.ui_density_dpi,
            dpi_customized: false,
            phone_audio: config.airplay.disable_audio_output
                && config.android_auto.disable_audio_output,
            echo_cancellation: false,
            noise_suppression: true,
            noise_suppression_level: prelude_media::NoiseSuppressionLevel::Moderate,
            high_pass_filter: true,
            gain_control: false,
            carplay_dock: config.airplay.dock,
            presets: config.platform.presets.actions(),
            equalizer: config.media.equalizer.settings(),
            wifi_channel: Some(config.wireless.channel),
            wifi_channel_width_mhz: Some(config.wireless.channel_width_mhz),
        }
    }

    pub fn wifi_radio(self) -> Option<(u8, ChannelWidth)> {
        Some((
            self.wifi_channel?,
            ChannelWidth::from_mhz(self.wifi_channel_width_mhz?)?,
        ))
    }

    pub fn validate(self) -> Result<Self, SettingsError> {
        self.equalizer
            .validate()
            .map_err(SettingsError::InvalidEqualizer)?;
        if !(prelude_android_auto_protocol::MIN_VIDEO_DENSITY_DPI
            ..=prelude_android_auto_protocol::MAX_VIDEO_DENSITY_DPI)
            .contains(&self.reported_dpi)
            || !(prelude_android_auto_protocol::MIN_VIDEO_DENSITY_DPI
                ..=prelude_android_auto_protocol::MAX_VIDEO_DENSITY_DPI)
                .contains(&self.original_dpi)
        {
            return Err(SettingsError::InvalidDpi {
                minimum: prelude_android_auto_protocol::MIN_VIDEO_DENSITY_DPI,
                maximum: prelude_android_auto_protocol::MAX_VIDEO_DENSITY_DPI,
            });
        }
        if let Some(channel) = self
            .wifi_channel
            .filter(|&channel| !is_supported_channel(channel))
        {
            return Err(SettingsError::InvalidWifiChannel(channel));
        }
        if let Some(width) = self
            .wifi_channel_width_mhz
            .filter(|&width| ChannelWidth::from_mhz(width).is_none())
        {
            return Err(SettingsError::InvalidWifiWidth(width));
        }
        Ok(self)
    }

    pub fn apply_to_config(self, config: &mut DaemonConfig) -> Result<(), SettingsError> {
        let settings = self.validate()?;
        config
            .media
            .equalizer
            .set(settings.equalizer)
            .map_err(SettingsError::InvalidEqualizer)?;
        settings.apply_to_config_preserving_equalizer(config)
    }

    pub fn apply_to_config_preserving_equalizer(
        self,
        config: &mut DaemonConfig,
    ) -> Result<(), SettingsError> {
        let settings = self.validate()?;
        config.platform.ambient_light.automatic_brightness = settings.auto_brightness;
        config.platform.ambient_light.automatic_night_mode = settings.auto_night_mode;
        config.android_auto.ui_density_dpi = settings.reported_dpi;
        config.airplay.disable_audio_output = settings.phone_audio;
        config.android_auto.disable_audio_output = settings.phone_audio;
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        prelude_media::set_echo_cancellation(settings.echo_tuning(config.media.audio));
        config.airplay.dock = settings.carplay_dock;
        config.platform.presets.set(settings.presets);
        if let Some(channel) = settings.wifi_channel {
            config.wireless.channel = channel;
        }
        if let Some(width) = settings.wifi_channel_width_mhz {
            config.wireless.channel_width_mhz = width;
        }
        let dpi_customized =
            settings.dpi_customized || settings.reported_dpi != settings.original_dpi;
        if dpi_customized {
            config.airplay.display_width_physical_mm =
                pixels_to_millimeters(800, settings.reported_dpi);
            config.airplay.display_height_physical_mm =
                pixels_to_millimeters(480, settings.reported_dpi);
        }
        Ok(())
    }

    fn use_echo_cancellation(&self, audio: crate::AudioBackend) -> bool {
        self.echo_cancellation && !self.phone_audio && audio == crate::AudioBackend::AlsaUsb
    }

    pub(crate) fn echo_knobs(&self) -> prelude_media::EchoTuning {
        self.echo_tuning(crate::AudioBackend::None)
    }

    pub(crate) fn echo_tuning(&self, audio: crate::AudioBackend) -> prelude_media::EchoTuning {
        prelude_media::EchoTuning {
            enabled: self.use_echo_cancellation(audio),
            noise_suppression: self.noise_suppression,
            noise_suppression_level: self.noise_suppression_level,
            high_pass_filter: self.high_pass_filter,
            gain_control: self.gain_control,
        }
    }

    pub fn load(state_dir: &Path, defaults: Self) -> Result<Self, SettingsError> {
        let path = state_dir.join(SETTINGS_FILE);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return defaults.validate();
            }
            Err(error) => return Err(SettingsError::Read(error)),
        };
        if !metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > MAXIMUM_SETTINGS_BYTES
            || metadata.permissions().mode() & 0o077 != 0
        {
            return defaults.validate();
        }
        let input = fs::read_to_string(path).map_err(SettingsError::Read)?;
        let mut settings = toml::from_str::<Self>(&input).map_err(SettingsError::Decode)?;
        settings.wifi_channel = settings.wifi_channel.or(defaults.wifi_channel);
        settings.wifi_channel_width_mhz = settings
            .wifi_channel_width_mhz
            .or(defaults.wifi_channel_width_mhz);
        settings.validate()
    }

    pub fn persist(self, state_dir: &Path) -> Result<(), SettingsError> {
        let settings = self.validate()?;
        fs::create_dir_all(state_dir).map_err(SettingsError::Write)?;
        let path = state_dir.join(SETTINGS_FILE);
        let temporary = state_dir.join(format!(".{SETTINGS_FILE}.tmp.{}", std::process::id()));
        let result = (|| {
            let contents = toml::to_string(&settings).map_err(SettingsError::Encode)?;
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(SettingsError::Write)?;
            file.write_all(contents.as_bytes())
                .and_then(|()| file.write_all(b"\n"))
                .map_err(SettingsError::Write)?;
            file.sync_all().map_err(SettingsError::Write)?;
            fs::rename(&temporary, &path).map_err(SettingsError::Write)?;
            fs::File::open(state_dir)
                .and_then(|directory| directory.sync_all())
                .map_err(SettingsError::Write)
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
}

fn pixels_to_millimeters(pixels: u32, dpi: u32) -> u16 {
    ((f64::from(pixels) * 25.4 / f64::from(dpi))
        .round()
        .clamp(1.0, 2_000.0)) as u16
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn settings_round_trip_atomically_and_privately() {
        let directory = TempDir::new().expect("state directory");
        let settings = RuntimeSettings {
            auto_brightness: false,
            auto_night_mode: true,
            reported_dpi: 220,
            original_dpi: 170,
            dpi_customized: true,
            phone_audio: true,
            echo_cancellation: true,
            noise_suppression: false,
            noise_suppression_level: prelude_media::NoiseSuppressionLevel::VeryHigh,
            high_pass_filter: false,
            gain_control: true,
            carplay_dock: CarPlayDock::Bottom,
            presets: [
                PresetAction::VoiceAssistant,
                PresetAction::AnswerCall,
                PresetAction::EndCall,
                PresetAction::None,
            ],
            equalizer: prelude_media::EqualizerSettings {
                enabled: true,
                bands_half_db: [24; 10],
                preamp_half_db: -48,
            },
            wifi_channel: Some(149),
            wifi_channel_width_mhz: Some(40),
        };
        settings.persist(directory.path()).expect("persist");
        assert_eq!(
            RuntimeSettings::load(directory.path(), RuntimeSettings::default()).expect("load"),
            settings
        );
        assert_eq!(
            fs::metadata(directory.path().join(SETTINGS_FILE))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn equalizer_legacy_validation_and_maximum_file_size() {
        let legacy: RuntimeSettings = toml::from_str("reported_dpi = 200\n").expect("legacy");
        assert_eq!(
            legacy.equalizer,
            prelude_media::EqualizerSettings::default()
        );
        let mut settings = RuntimeSettings {
            reported_dpi: prelude_android_auto_protocol::MAX_VIDEO_DENSITY_DPI,
            original_dpi: prelude_android_auto_protocol::MAX_VIDEO_DENSITY_DPI,
            equalizer: prelude_media::EqualizerSettings {
                enabled: true,
                bands_half_db: [-24; 10],
                preamp_half_db: -48,
            },
            wifi_channel: Some(149),
            wifi_channel_width_mhz: Some(80),
            ..Default::default()
        };
        assert!(
            toml::to_string(&settings).expect("encode").len() < MAXIMUM_SETTINGS_BYTES as usize
        );
        settings.equalizer.preamp_half_db = 25;
        assert!(matches!(
            settings.validate(),
            Err(SettingsError::InvalidEqualizer(_))
        ));
    }

    #[test]
    fn disabled_equalizer_roundtrips_the_saved_curve_and_shared_config_keeps_latest() {
        let directory = TempDir::new().expect("directory");
        let saved = prelude_media::EqualizerSettings {
            enabled: false,
            bands_half_db: [-24, 24, 0, 1, 2, 3, 4, 5, 6, 7],
            preamp_half_db: -48,
        };
        let settings = RuntimeSettings {
            equalizer: saved,
            ..Default::default()
        };
        settings.persist(directory.path()).expect("save");
        let loaded = RuntimeSettings::load(directory.path(), Default::default()).expect("load");
        assert_eq!(loaded.equalizer, saved);
        let mut config = DaemonConfig::default();
        let before_session = config.media.clone();
        loaded.apply_to_config(&mut config).expect("apply");
        assert_eq!(before_session.equalizer.settings(), saved);
        let mut next = loaded;
        next.equalizer.enabled = true;
        next.apply_to_config(&mut config).expect("rebuild");
        assert!(before_session.equalizer.settings().enabled);
        assert_eq!(
            config.media.clone().equalizer.settings().bands_half_db,
            saved.bands_half_db
        );
    }

    #[test]
    fn stale_restart_snapshot_does_not_revert_a_later_live_equalizer_edit() {
        let mut config = DaemonConfig::default();
        let snapshot = RuntimeSettings {
            phone_audio: true,
            ..Default::default()
        };
        let latest = prelude_media::EqualizerSettings {
            enabled: true,
            bands_half_db: [7; 10],
            preamp_half_db: -20,
        };
        let existing_session = config.media.clone();
        config
            .media
            .equalizer
            .set(latest)
            .expect("live edit after event queued");
        snapshot
            .apply_to_config_preserving_equalizer(&mut config)
            .expect("runtime rebuild");
        assert!(config.airplay.disable_audio_output);
        assert_eq!(config.media.equalizer.settings(), latest);
        assert_eq!(existing_session.equalizer.settings(), latest);
    }

    #[test]
    fn missing_or_unsafe_settings_use_defaults() {
        let directory = TempDir::new().expect("state directory");
        let defaults = RuntimeSettings {
            auto_brightness: false,
            ..RuntimeSettings::default()
        };
        assert_eq!(
            RuntimeSettings::load(directory.path(), defaults).expect("missing"),
            defaults
        );

        fs::write(directory.path().join("target"), "reported_dpi = 240\n").expect("target");
        symlink(
            directory.path().join("target"),
            directory.path().join(SETTINGS_FILE),
        )
        .expect("symlink");
        assert_eq!(
            RuntimeSettings::load(directory.path(), defaults).expect("unsafe"),
            defaults
        );
    }

    #[test]
    fn out_of_range_dpi_is_rejected() {
        let settings = RuntimeSettings {
            reported_dpi: prelude_android_auto_protocol::MAX_VIDEO_DENSITY_DPI + 1,
            ..RuntimeSettings::default()
        };
        assert!(matches!(
            settings.validate(),
            Err(SettingsError::InvalidDpi { .. })
        ));
    }

    #[test]
    fn customized_dpi_updates_both_projection_protocols() {
        let mut config = DaemonConfig::default();
        RuntimeSettings {
            reported_dpi: 170,
            dpi_customized: true,
            ..RuntimeSettings::default()
        }
        .apply_to_config(&mut config)
        .expect("apply settings");
        assert_eq!(config.android_auto.ui_density_dpi, 170);
        assert_eq!(config.airplay.display_width_physical_mm, 120);
        assert_eq!(config.airplay.display_height_physical_mm, 72);
    }

    #[test]
    fn phone_audio_disables_audio_output_for_both_projection_protocols() {
        let mut config = DaemonConfig::default();
        RuntimeSettings {
            phone_audio: true,
            ..RuntimeSettings::default()
        }
        .apply_to_config(&mut config)
        .expect("apply settings");
        assert!(config.airplay.disable_audio_output);
        assert!(config.android_auto.disable_audio_output);

        RuntimeSettings::default()
            .apply_to_config(&mut config)
            .expect("apply settings");
        assert!(!config.airplay.disable_audio_output);
        assert!(!config.android_auto.disable_audio_output);
    }

    #[test]
    fn carplay_dock_is_applied_and_defaults_to_left_for_legacy_settings() {
        let legacy: RuntimeSettings = toml::from_str("reported_dpi = 200\n").expect("legacy");
        assert_eq!(legacy.carplay_dock, CarPlayDock::Left);
        let decoded: RuntimeSettings =
            toml::from_str("carplay_dock = \"right\"\n").expect("decode");
        assert_eq!(decoded.carplay_dock, CarPlayDock::Right);
        let mut config = DaemonConfig::default();
        for dock in CarPlayDock::ALL {
            RuntimeSettings {
                carplay_dock: dock,
                ..RuntimeSettings::default()
            }
            .apply_to_config(&mut config)
            .expect("apply settings");
            assert_eq!(config.airplay.dock, dock);
        }
    }

    #[test]
    fn presets_are_applied_live_and_default_to_the_legacy_layout() {
        let legacy: RuntimeSettings = toml::from_str("reported_dpi = 200\n").expect("legacy");
        assert_eq!(legacy.presets, PresetAction::DEFAULTS);
        let decoded: RuntimeSettings =
            toml::from_str("presets = [\"home\", \"back\", \"voice_assistant\", \"end_call\"]\n")
                .expect("decode");
        let mut config = DaemonConfig::default();
        let shared = config.platform.presets.clone();
        decoded
            .apply_to_config(&mut config)
            .expect("apply settings");
        assert_eq!(shared.actions(), decoded.presets);
        assert!(toml::from_str::<RuntimeSettings>("presets = [\"home\"]\n").is_err());
        assert!(
            toml::from_str::<RuntimeSettings>("presets = [\"x\", \"x\", \"x\", \"x\"]\n").is_err()
        );
    }

    #[test]
    fn settings_without_phone_audio_keep_the_usb_dac() {
        let settings: RuntimeSettings = toml::from_str("reported_dpi = 200\n").expect("decode");
        assert!(!settings.phone_audio);
        assert!(!settings.echo_cancellation);
    }

    #[test]
    fn echo_cancellation_runs_only_for_dac_calls_with_phone_audio_off() {
        let on = RuntimeSettings {
            echo_cancellation: true,
            ..RuntimeSettings::default()
        };
        assert!(on.use_echo_cancellation(crate::AudioBackend::AlsaUsb));
        assert!(!on.use_echo_cancellation(crate::AudioBackend::None));
        assert!(!on.use_echo_cancellation(crate::AudioBackend::BuiltInSuperbird));
        let phone_audio = RuntimeSettings {
            echo_cancellation: true,
            phone_audio: true,
            ..RuntimeSettings::default()
        };
        assert!(!phone_audio.use_echo_cancellation(crate::AudioBackend::AlsaUsb));
        assert!(!RuntimeSettings::default().use_echo_cancellation(crate::AudioBackend::AlsaUsb));
    }

    #[test]
    fn echo_knobs_default_when_absent_and_apply_only_while_echo_is_armed() {
        let legacy: RuntimeSettings = toml::from_str("reported_dpi = 200\n").expect("legacy");
        assert!(legacy.noise_suppression);
        assert_eq!(
            legacy.noise_suppression_level,
            prelude_media::NoiseSuppressionLevel::Moderate
        );
        assert!(legacy.high_pass_filter);
        assert!(!legacy.gain_control);
        assert!(
            legacy
                .echo_tuning(crate::AudioBackend::AlsaUsb)
                .noise_level()
                .is_none()
        );

        let tuned = RuntimeSettings {
            echo_cancellation: true,
            noise_suppression: true,
            noise_suppression_level: prelude_media::NoiseSuppressionLevel::High,
            high_pass_filter: false,
            gain_control: true,
            ..RuntimeSettings::default()
        };
        let armed = tuned.echo_tuning(crate::AudioBackend::AlsaUsb);
        assert!(armed.enabled);
        assert_eq!(
            armed.noise_level(),
            Some(prelude_media::NoiseSuppressionLevel::High)
        );
        assert!(!armed.high_pass_filter);
        assert!(armed.gain_control);

        let noise_off = RuntimeSettings {
            noise_suppression: false,
            ..tuned
        };
        assert!(
            noise_off
                .echo_tuning(crate::AudioBackend::AlsaUsb)
                .noise_level()
                .is_none()
        );
        for audio in [
            crate::AudioBackend::None,
            crate::AudioBackend::BuiltInSuperbird,
        ] {
            assert!(tuned.echo_tuning(audio).noise_level().is_none());
        }
        let phone_audio = RuntimeSettings {
            phone_audio: true,
            ..tuned
        };
        assert!(
            phone_audio
                .echo_tuning(crate::AudioBackend::AlsaUsb)
                .noise_level()
                .is_none()
        );
    }

    #[test]
    fn legacy_settings_keep_the_configured_wifi_radio() {
        let directory = TempDir::new().expect("state directory");
        let path = directory.path().join(SETTINGS_FILE);
        fs::write(&path, "reported_dpi = 200\n").expect("legacy settings");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("permissions");
        let mut config = DaemonConfig::default();
        config.wireless.channel = 44;
        let settings =
            RuntimeSettings::load(directory.path(), RuntimeSettings::from_config(&config))
                .expect("load");
        assert_eq!(settings.reported_dpi, 200);
        assert_eq!(settings.wifi_radio(), Some((44, ChannelWidth::Mhz20)));
    }

    #[test]
    fn wifi_radio_is_validated_and_applied() {
        for settings in [
            RuntimeSettings {
                wifi_channel: Some(52),
                ..RuntimeSettings::default()
            },
            RuntimeSettings {
                wifi_channel_width_mhz: Some(160),
                ..RuntimeSettings::default()
            },
        ] {
            assert!(settings.validate().is_err(), "{settings:?}");
        }
        let mut config = DaemonConfig::default();
        RuntimeSettings {
            wifi_channel: Some(11),
            wifi_channel_width_mhz: Some(40),
            ..RuntimeSettings::default()
        }
        .apply_to_config(&mut config)
        .expect("apply settings");
        assert_eq!(config.wireless.channel, 11);
        assert_eq!(config.wireless.channel_width_mhz, 40);
    }

    #[test]
    fn changed_dpi_updates_carplay_for_legacy_settings_without_customized_flag() {
        let mut config = DaemonConfig::default();
        RuntimeSettings {
            reported_dpi: 220,
            original_dpi: 170,
            dpi_customized: false,
            ..RuntimeSettings::default()
        }
        .apply_to_config(&mut config)
        .expect("apply settings");

        assert_eq!(config.android_auto.ui_density_dpi, 220);
        assert_eq!(config.airplay.display_width_physical_mm, 92);
        assert_eq!(config.airplay.display_height_physical_mm, 55);
    }
}
