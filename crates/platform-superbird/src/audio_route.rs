use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use thiserror::Error;

const MAXIMUM_STATE_BYTES: u64 = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioRoutePreference {
    UsbDac,
}

impl AudioRoutePreference {
    pub fn encode(self) -> String {
        match self {
            Self::UsbDac => "version=1\nroute=usb_dac\n".to_owned(),
        }
    }

    pub fn parse(value: &str) -> Result<Self, AudioRouteStateError> {
        if value == "version=1\nroute=usb_dac\n" {
            return Ok(Self::UsbDac);
        }
        Err(AudioRouteStateError::Malformed)
    }

    pub fn load(path: &Path) -> Result<Option<Self>, AudioRouteStateError> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(AudioRouteStateError::Io(error)),
        };
        if !metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > MAXIMUM_STATE_BYTES
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(AudioRouteStateError::UnsafeFile);
        }
        let value = fs::read_to_string(path).map_err(AudioRouteStateError::Io)?;
        Self::parse(&value).map(Some)
    }

    pub fn save(self, path: &Path) -> Result<(), AudioRouteStateError> {
        let directory = path.parent().ok_or(AudioRouteStateError::UnsafePath)?;
        fs::create_dir_all(directory).map_err(AudioRouteStateError::Io)?;
        let temporary = temporary_path(path)?;
        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(self.encode().as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            fs::File::open(directory)?.sync_all()?;
            Ok::<(), std::io::Error>(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(AudioRouteStateError::Io)
    }
}

fn temporary_path(path: &Path) -> Result<PathBuf, AudioRouteStateError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(AudioRouteStateError::UnsafePath)?;
    Ok(path.with_file_name(format!(".{name}.tmp.{}", std::process::id())))
}

#[derive(Debug, Error)]
pub enum AudioRouteStateError {
    #[error("audio route state I/O failed: {0}")]
    Io(#[source] std::io::Error),
    #[error("audio route state path is unsafe")]
    UnsafePath,
    #[error("audio route state file type, size, or permissions are unsafe")]
    UnsafeFile,
    #[error("audio route state is malformed")]
    Malformed,
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn usb_dac_preference_is_canonical_atomic_and_private() {
        let temporary = TempDir::new().expect("tempdir");
        let path = temporary.path().join("audio-route");
        AudioRoutePreference::UsbDac.save(&path).expect("save");
        assert_eq!(
            AudioRoutePreference::load(&path).expect("load"),
            Some(AudioRoutePreference::UsbDac)
        );
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::read_to_string(&path).expect("state"),
            "version=1\nroute=usb_dac\n"
        );
    }

    #[test]
    fn malformed_loose_and_symlink_state_fail_closed() {
        let temporary = TempDir::new().expect("tempdir");
        let path = temporary.path().join("audio-route");
        fs::write(&path, "version=1\nroute=unsupported\n").expect("obsolete route");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
        assert!(matches!(
            AudioRoutePreference::load(&path),
            Err(AudioRouteStateError::Malformed)
        ));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("mode");
        assert!(matches!(
            AudioRoutePreference::load(&path),
            Err(AudioRouteStateError::UnsafeFile)
        ));
        fs::remove_file(&path).expect("remove");
        let target = temporary.path().join("target");
        fs::write(&target, AudioRoutePreference::UsbDac.encode()).expect("target");
        symlink(&target, &path).expect("symlink");
        assert!(matches!(
            AudioRoutePreference::load(&path),
            Err(AudioRouteStateError::UnsafeFile)
        ));
    }
}
