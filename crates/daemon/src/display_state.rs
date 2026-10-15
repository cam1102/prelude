use std::{
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
    Connecting,
    Connected,
    MissingAudioOutput,
}

impl DisplayPlatform {
    pub const fn overlay_path(self) -> &'static str {
        self.status_overlay_path(DisplayStatus::WaitingForPhone)
    }

    pub const fn status_overlay_path(self, status: DisplayStatus) -> &'static str {
        match (self, status) {
            (Self::CarPlay, DisplayStatus::WaitingForPhone) => "/usr/share/prelude/carplay.nv12",
            (Self::CarPlay, DisplayStatus::UnlockPhone) => {
                "/usr/share/prelude/carplay-unlock-phone.nv12"
            }
            (Self::CarPlay, DisplayStatus::Connecting) => {
                "/usr/share/prelude/carplay-connecting.nv12"
            }
            (Self::CarPlay, DisplayStatus::Connected) => {
                "/usr/share/prelude/carplay-connected.nv12"
            }
            (Self::CarPlay, DisplayStatus::MissingAudioOutput) => {
                "/usr/share/prelude/carplay-missing-audio.nv12"
            }
            (Self::AndroidAuto, DisplayStatus::WaitingForPhone) => {
                "/usr/share/prelude/android-auto.nv12"
            }
            (Self::AndroidAuto, DisplayStatus::UnlockPhone)
            | (Self::AndroidAuto, DisplayStatus::Connecting) => {
                "/usr/share/prelude/android-auto-connecting.nv12"
            }
            (Self::AndroidAuto, DisplayStatus::Connected) => {
                "/usr/share/prelude/android-auto-connected.nv12"
            }
            (Self::AndroidAuto, DisplayStatus::MissingAudioOutput) => {
                "/usr/share/prelude/android-auto-missing-audio.nv12"
            }
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
    fn status_overlay_paths_are_platform_specific() {
        assert_eq!(
            DisplayPlatform::CarPlay.status_overlay_path(DisplayStatus::UnlockPhone),
            "/usr/share/prelude/carplay-unlock-phone.nv12"
        );
        assert_eq!(
            DisplayPlatform::AndroidAuto.status_overlay_path(DisplayStatus::Connected),
            "/usr/share/prelude/android-auto-connected.nv12"
        );
        assert_eq!(
            DisplayPlatform::CarPlay.status_overlay_path(DisplayStatus::MissingAudioOutput),
            "/usr/share/prelude/carplay-missing-audio.nv12"
        );
    }
}
