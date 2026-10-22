use std::{
    borrow::Cow,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

const LAST_PLATFORM_FILE: &str = "last-platform";
const MAXIMUM_STATE_BYTES: u64 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayPlatform {
    CarPlay,
    AndroidAuto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayStatus {
    WaitingForPhone,
    UnlockPhone,
    Connecting(Option<ConnectionStep>),
    ConfirmPairing(u32),
    Connected,
    MissingAudioOutput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStep {
    AccessoryMode,
    OpeningConnection,
    ExchangingVersions,
    SecuringConnection,
    DiscoveringServices,
    OpeningChannels,
    StartingMedia,
    StartingUsbNetwork,
    StartingWifi,
    PreparingAuthentication,
    StartingReceiver,
    Authenticating,
    WaitingForCarPlay,
    StartingCarPlay,
    WaitingForIphone,
    WaitingForIphoneOnWifi,
    VerifyingPairing,
    SettingUpSession,
    SettingUpStreams,
    StartingStream,
    SwitchingToUsb,
}

impl ConnectionStep {
    pub const fn label(self) -> &'static str {
        match self {
            Self::AccessoryMode => "Switching phone to accessory mode",
            Self::OpeningConnection => "Opening connection",
            Self::ExchangingVersions => "Exchanging protocol versions",
            Self::SecuringConnection => "Securing connection",
            Self::DiscoveringServices => "Discovering services",
            Self::OpeningChannels => "Opening channels",
            Self::StartingMedia => "Starting audio and video",
            Self::StartingUsbNetwork => "Starting USB network",
            Self::StartingWifi => "Starting Wi-Fi",
            Self::PreparingAuthentication => "Preparing authentication",
            Self::StartingReceiver => "Starting CarPlay receiver",
            Self::Authenticating => "Authenticating",
            Self::WaitingForCarPlay => "Waiting for iPhone to offer CarPlay",
            Self::StartingCarPlay => "Starting CarPlay",
            Self::WaitingForIphone => "Waiting for iPhone",
            Self::WaitingForIphoneOnWifi => "Waiting for iPhone to join Wi-Fi",
            Self::VerifyingPairing => "Verifying pairing",
            Self::SettingUpSession => "Setting up session",
            Self::SettingUpStreams => "Setting up audio and video",
            Self::StartingStream => "Starting stream",
            Self::SwitchingToUsb => "Switching to USB",
        }
    }
}

impl DisplayStatus {
    pub const fn headline(self) -> &'static str {
        match self {
            Self::WaitingForPhone => "Waiting for phone…",
            Self::UnlockPhone => "Unlock your phone",
            Self::Connecting(_) | Self::ConfirmPairing(_) => "Connecting…",
            Self::Connected => "Connected!",
            Self::MissingAudioOutput => "Missing audio output device",
        }
    }

    pub fn detail(self) -> Option<Cow<'static, str>> {
        match self {
            Self::Connecting(Some(step)) => Some(Cow::Borrowed(step.label())),
            Self::ConfirmPairing(passkey) => Some(Cow::Owned(format!("Pairing code {passkey:06}"))),
            Self::Connected => Some(Cow::Borrowed("Waiting for video")),
            Self::WaitingForPhone
            | Self::UnlockPhone
            | Self::Connecting(None)
            | Self::MissingAudioOutput => None,
        }
    }
}

impl DisplayPlatform {
    pub const fn logo_path(self) -> &'static str {
        match self {
            Self::CarPlay => "/usr/share/prelude/carplay-logo.nv12",
            Self::AndroidAuto => "/usr/share/prelude/android-auto-logo.nv12",
        }
    }

    const fn encoded(self) -> &'static str {
        match self {
            Self::CarPlay => "version=1\nplatform=carplay\n",
            Self::AndroidAuto => "version=1\nplatform=android_auto\n",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "version=1\nplatform=carplay\n" => Some(Self::CarPlay),
            "version=1\nplatform=android_auto\n" => Some(Self::AndroidAuto),
            _ => None,
        }
    }
}

pub fn load_last_display_platform(state_dir: &Path) -> Option<DisplayPlatform> {
    let path = state_dir.join(LAST_PLATFORM_FILE);
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_file()
        || metadata.len() > MAXIMUM_STATE_BYTES
        || metadata.permissions().mode() & 0o077 != 0
    {
        return None;
    }
    fs::read_to_string(path)
        .ok()
        .and_then(|value| DisplayPlatform::parse(&value))
}

pub fn persist_last_display_platform(
    state_dir: &Path,
    platform: DisplayPlatform,
) -> std::io::Result<()> {
    fs::create_dir_all(state_dir)?;
    let path = state_dir.join(LAST_PLATFORM_FILE);
    let temporary = state_dir.join(format!(".{LAST_PLATFORM_FILE}.tmp.{}", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(platform.encoded().as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, &path)?;
        fs::File::open(state_dir)?.sync_all()?;
        Ok::<(), std::io::Error>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn missing_or_malformed_state_defaults_to_carplay() {
        let temporary = TempDir::new().expect("tempdir");
        assert_eq!(load_last_display_platform(temporary.path()), None);
        fs::write(
            temporary.path().join(LAST_PLATFORM_FILE),
            "version=1\nplatform=unknown\n",
        )
        .expect("malformed state");
        assert_eq!(load_last_display_platform(temporary.path()), None);
    }

    #[test]
    fn platform_state_is_atomic_private_and_round_trips() {
        let temporary = TempDir::new().expect("tempdir");
        persist_last_display_platform(temporary.path(), DisplayPlatform::AndroidAuto)
            .expect("save");
        let path = temporary.path().join(LAST_PLATFORM_FILE);
        assert_eq!(
            load_last_display_platform(temporary.path()),
            Some(DisplayPlatform::AndroidAuto)
        );
        assert_eq!(
            fs::metadata(path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn symlink_state_defaults_to_carplay() {
        let temporary = TempDir::new().expect("tempdir");
        let target = temporary.path().join("target");
        fs::write(&target, DisplayPlatform::AndroidAuto.encoded()).expect("target");
        symlink(target, temporary.path().join(LAST_PLATFORM_FILE)).expect("symlink");
        assert_eq!(load_last_display_platform(temporary.path()), None);
    }

    #[test]
    fn only_progress_statuses_carry_a_detail_line() {
        assert_eq!(DisplayStatus::WaitingForPhone.detail(), None);
        assert_eq!(DisplayStatus::Connecting(None).detail(), None);
        assert_eq!(
            DisplayStatus::Connecting(Some(ConnectionStep::SecuringConnection))
                .detail()
                .as_deref(),
            Some("Securing connection")
        );
        assert_eq!(
            DisplayStatus::Connecting(Some(ConnectionStep::Authenticating)).headline(),
            DisplayStatus::Connecting(None).headline()
        );
        assert_eq!(
            DisplayStatus::Connected.detail().as_deref(),
            Some("Waiting for video")
        );
        assert_eq!(
            DisplayStatus::ConfirmPairing(4_217).detail().as_deref(),
            Some("Pairing code 004217")
        );
        assert_eq!(
            DisplayStatus::ConfirmPairing(4_217).headline(),
            DisplayStatus::Connecting(None).headline()
        );
    }
}
