use std::{
    fmt, fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use thiserror::Error;

const DEFAULT_DEVICE_TREE_ROOT: &str = "/proc/device-tree";
const DEFAULT_MAX_PROPERTY_BYTES: usize = 256;
const EXPECTED_MODEL: &str = "Spotify Car Thing (Superbird)";

#[derive(Clone, PartialEq, Eq)]
pub struct AccessoryIdentity {
    pub name: String,
    pub model_identifier: String,
    pub manufacturer: String,
    pub serial_number: String,
    pub firmware_version: String,
    pub hardware_version: String,
}

impl fmt::Debug for AccessoryIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccessoryIdentity")
            .field("name", &self.name)
            .field("model_identifier", &self.model_identifier)
            .field("manufacturer", &self.manufacturer)
            .field("serial_number", &"[redacted]")
            .field("firmware_version", &self.firmware_version)
            .field("hardware_version", &self.hardware_version)
            .finish()
    }
}

pub trait AccessoryIdentitySource: fmt::Debug + Send + Sync {
    fn load(&self) -> Result<AccessoryIdentity, IdentityError>;
}

#[derive(Debug, Clone)]
pub struct SuperbirdIdentitySource {
    device_tree_root: PathBuf,
    firmware_version: String,
    maximum_property_bytes: usize,
}

impl SuperbirdIdentitySource {
    pub fn new(firmware_version: impl Into<String>) -> Self {
        Self {
            device_tree_root: PathBuf::from(DEFAULT_DEVICE_TREE_ROOT),
            firmware_version: firmware_version.into(),
            maximum_property_bytes: DEFAULT_MAX_PROPERTY_BYTES,
        }
    }

    pub fn with_device_tree_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.device_tree_root = root.into();
        self
    }

    pub fn with_maximum_property_bytes(mut self, maximum: usize) -> Self {
        self.maximum_property_bytes = maximum;
        self
    }
}

impl AccessoryIdentitySource for SuperbirdIdentitySource {
    fn load(&self) -> Result<AccessoryIdentity, IdentityError> {
        if self.maximum_property_bytes == 0 {
            return Err(IdentityError::InvalidLimit);
        }
        if self.firmware_version.trim().is_empty() || self.firmware_version.as_bytes().contains(&0)
        {
            return Err(IdentityError::InvalidFirmwareVersion);
        }
        let model = read_device_tree_string(
            &self.device_tree_root.join("model"),
            self.maximum_property_bytes,
        )?;
        if model != EXPECTED_MODEL {
            return Err(IdentityError::UnexpectedModel);
        }
        let serial_number = read_device_tree_string(
            &self.device_tree_root.join("serial-number"),
            self.maximum_property_bytes,
        )?;
        Ok(AccessoryIdentity {
            name: "Prelude".to_owned(),
            model_identifier: "Prelude".to_owned(),
            manufacturer: "Vanta Labs".to_owned(),
            serial_number,
            firmware_version: self.firmware_version.clone(),
            hardware_version: "Prelude".to_owned(),
        })
    }
}

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("device-tree property {path} could not be read: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("device-tree property {path} exceeds the configured size limit")]
    PropertyTooLarge { path: PathBuf },
    #[error("device-tree property {path} is not a canonical non-empty NUL-terminated UTF-8 string")]
    InvalidProperty { path: PathBuf },
    #[error("device-tree model is not the Spotify Car Thing Superbird target")]
    UnexpectedModel,
    #[error("device-tree property size limit must be non-zero")]
    InvalidLimit,
    #[error("compiled daemon version is empty or contains NUL")]
    InvalidFirmwareVersion,
}

fn read_device_tree_string(path: &Path, maximum: usize) -> Result<String, IdentityError> {
    let mut file = fs::File::open(path).map_err(|source| IdentityError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let take_limit = u64::try_from(maximum).unwrap_or(u64::MAX).saturating_add(1);
    let mut bytes = Vec::with_capacity(maximum.min(DEFAULT_MAX_PROPERTY_BYTES));
    file.by_ref()
        .take(take_limit)
        .read_to_end(&mut bytes)
        .map_err(|source| IdentityError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > maximum {
        return Err(IdentityError::PropertyTooLarge {
            path: path.to_path_buf(),
        });
    }
    let Some(content) = bytes.strip_suffix(&[0]) else {
        return Err(IdentityError::InvalidProperty {
            path: path.to_path_buf(),
        });
    };
    if content.is_empty() || content.contains(&0) {
        return Err(IdentityError::InvalidProperty {
            path: path.to_path_buf(),
        });
    }
    let value = std::str::from_utf8(content).map_err(|_| IdentityError::InvalidProperty {
        path: path.to_path_buf(),
    })?;
    if value.chars().any(char::is_control) {
        return Err(IdentityError::InvalidProperty {
            path: path.to_path_buf(),
        });
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn source(root: &Path) -> SuperbirdIdentitySource {
        SuperbirdIdentitySource::new("0.1.0").with_device_tree_root(root)
    }

    fn write_identity(root: &Path, serial: &[u8]) {
        fs::write(root.join("model"), format!("{EXPECTED_MODEL}\0")).expect("model");
        fs::write(root.join("serial-number"), serial).expect("serial");
    }

    #[test]
    fn loads_factual_superbird_identity_and_redacts_serial() {
        let root = TempDir::new().expect("tempdir");
        write_identity(root.path(), b"actual-unit-serial\0");
        let identity = source(root.path()).load().expect("identity");
        assert_eq!(identity.name, "Prelude");
        assert_eq!(identity.model_identifier, "Prelude");
        assert_eq!(identity.manufacturer, "Vanta Labs");
        assert_eq!(identity.serial_number, "actual-unit-serial");
        assert_eq!(identity.firmware_version, "0.1.0");
        assert_eq!(identity.hardware_version, "Prelude");
        assert!(!format!("{identity:?}").contains("actual-unit-serial"));
    }

    #[test]
    fn rejects_unterminated_embedded_nul_and_non_utf8_properties() {
        let root = TempDir::new().expect("tempdir");
        for invalid in [
            b"unterminated".as_slice(),
            b"embedded\0value\0".as_slice(),
            &[0xff, 0],
        ] {
            write_identity(root.path(), invalid);
            assert!(matches!(
                source(root.path()).load(),
                Err(IdentityError::InvalidProperty { .. })
            ));
        }
    }

    #[test]
    fn rejects_oversized_property_without_unbounded_read() {
        let root = TempDir::new().expect("tempdir");
        write_identity(root.path(), b"12345678\0");
        assert!(matches!(
            source(root.path()).with_maximum_property_bytes(4).load(),
            Err(IdentityError::PropertyTooLarge { .. })
        ));
    }

    #[test]
    fn refuses_a_different_device_tree_model() {
        let root = TempDir::new().expect("tempdir");
        write_identity(root.path(), b"serial\0");
        fs::write(root.path().join("model"), b"Different board\0").expect("model");
        assert!(matches!(
            source(root.path()).load(),
            Err(IdentityError::UnexpectedModel)
        ));
    }
}
