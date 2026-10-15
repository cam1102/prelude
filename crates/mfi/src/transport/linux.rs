use std::{
    fs::{self, OpenOptions},
    io,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use fs2::FileExt;
use i2cdev::{core::I2CDevice, linux::LinuxI2CDevice};

use super::Transport;
use crate::TransportError;

const RETRY_LIMIT: usize = 3;
const RETRY_DELAY: Duration = Duration::from_micros(500);
const LOCK_WAIT: Duration = Duration::from_secs(2);
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxI2cConfig {
    pub device: PathBuf,
    pub address: u16,
    pub lock_file: PathBuf,
}

impl LinuxI2cConfig {
    pub const DEFAULT_ADDRESS: u16 = 0x10;

    pub fn new(device: impl Into<PathBuf>, address: u16) -> Self {
        Self {
            device: device.into(),
            address,
            lock_file: PathBuf::from("/run/prelude/mfi.lock"),
        }
    }

    pub fn at(device: impl Into<PathBuf>) -> Self {
        Self::new(device, Self::DEFAULT_ADDRESS)
    }

    pub fn with_lock_file(mut self, lock_file: impl Into<PathBuf>) -> Self {
        self.lock_file = lock_file.into();
        self
    }
}

impl Default for LinuxI2cConfig {
    fn default() -> Self {
        Self::at("/dev/i2c-3")
    }
}

#[derive(Debug)]
pub struct LinuxI2c {
    device: LinuxI2CDevice,
    _lock: fs::File,
}

impl LinuxI2c {
    pub fn open(config: &LinuxI2cConfig) -> std::result::Result<Self, TransportError> {
        let lock = acquire_lock(&config.lock_file)?;
        let device = LinuxI2CDevice::new(&config.device, config.address).map_err(map_error)?;
        Ok(Self {
            device,
            _lock: lock,
        })
    }

    pub fn open_path(path: impl AsRef<Path>) -> std::result::Result<Self, TransportError> {
        Self::open(&LinuxI2cConfig::at(path.as_ref().to_path_buf()))
    }
}

fn acquire_lock(path: &Path) -> std::result::Result<fs::File, TransportError> {
    acquire_lock_with_timeout(path, LOCK_WAIT)
}

fn acquire_lock_with_timeout(
    path: &Path,
    timeout: Duration,
) -> std::result::Result<fs::File, TransportError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(TransportError::Protocol(format!(
                        "MFi coprocessor is busy; could not acquire {} within {timeout:?}",
                        path.display()
                    )));
                }
                thread::sleep(LOCK_RETRY_DELAY);
            }
            Err(error) => return Err(TransportError::Io(error)),
        }
    }
}

impl Transport for LinuxI2c {
    fn select(&mut self, register: u8) -> std::result::Result<(), TransportError> {
        retry(|| self.device.smbus_write_byte(register).map_err(map_error))
    }

    fn write(&mut self, register: u8, data: &[u8]) -> std::result::Result<(), TransportError> {
        if data.len() > 32 {
            return Err(TransportError::Protocol(format!(
                "SMBus block write of {} bytes exceeds the 32-byte limit",
                data.len()
            )));
        }
        retry(|| {
            self.device
                .smbus_write_i2c_block_data(register, data)
                .map_err(map_error)
        })
    }

    fn read_exact(&mut self, out: &mut [u8]) -> std::result::Result<(), TransportError> {
        retry(|| self.device.read(out).map_err(map_error))
    }
}

fn retry<T>(
    mut operation: impl FnMut() -> std::result::Result<T, TransportError>,
) -> std::result::Result<T, TransportError> {
    let mut last_error = None;
    for attempt in 0..RETRY_LIMIT {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) => {
                last_error = Some(error);
                if attempt + 1 < RETRY_LIMIT {
                    thread::sleep(RETRY_DELAY);
                }
            }
        }
    }
    Err(last_error
        .unwrap_or_else(|| TransportError::Protocol("I2C operation was not attempted".into())))
}

fn map_error(error: i2cdev::linux::LinuxI2CError) -> TransportError {
    match error {
        i2cdev::linux::LinuxI2CError::Errno(errno) => {
            TransportError::Io(io::Error::from_raw_os_error(errno))
        }
        i2cdev::linux::LinuxI2CError::Io(error) => TransportError::Io(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_lock_times_out_and_is_released_with_owner() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("mfi.lock");
        let first =
            acquire_lock_with_timeout(&path, Duration::from_millis(30)).expect("first lock owner");

        assert!(matches!(
            acquire_lock_with_timeout(&path, Duration::from_millis(30)),
            Err(TransportError::Protocol(message)) if message.contains("busy")
        ));
        drop(first);
        acquire_lock_with_timeout(&path, Duration::from_millis(30))
            .expect("lock released with owner");
    }

    #[test]
    fn default_lock_lives_in_the_daemon_writable_runtime_directory() {
        assert_eq!(
            LinuxI2cConfig::default().lock_file,
            PathBuf::from("/run/prelude/mfi.lock")
        );
    }
}
