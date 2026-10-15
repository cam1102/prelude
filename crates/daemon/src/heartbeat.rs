use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use crate::{Error, Result};
use platform_superbird::HeartbeatStamp;

#[derive(Debug, Clone)]
pub struct HeartbeatWriter {
    state_dir: PathBuf,
    path: PathBuf,
}

impl HeartbeatWriter {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        let state_dir = state_dir.into();
        let path = state_dir.join("daemon-heartbeat");
        Self { state_dir, path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write(&self) -> Result<()> {
        fs::create_dir_all(&self.state_dir).map_err(|source| Error::Heartbeat {
            path: self.path.clone(),
            source,
        })?;
        let temporary = self
            .state_dir
            .join(format!(".daemon-heartbeat.{}.tmp", std::process::id()));
        let result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&temporary)?;
            file.write_all(
                &HeartbeatStamp::current()
                    .map_err(std::io::Error::other)?
                    .encode(),
            )?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            Ok(())
        })();
        if let Err(source) = result {
            let _ = fs::remove_file(&temporary);
            return Err(Error::Heartbeat {
                path: self.path.clone(),
                source,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_is_written_atomically_in_state_directory() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let heartbeat = HeartbeatWriter::new(directory.path());
        heartbeat.write().expect("heartbeat");
        heartbeat.write().expect("second heartbeat");
        let value = fs::read_to_string(heartbeat.path()).expect("heartbeat content");
        HeartbeatStamp::decode(value.as_bytes()).expect("heartbeat stamp");
        let files: Vec<_> = fs::read_dir(directory.path())
            .expect("state directory")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(files, ["daemon-heartbeat"]);
    }

    #[test]
    fn heartbeat_fails_when_state_directory_is_a_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let state_file = directory.path().join("not-a-directory");
        fs::write(&state_file, b"occupied").expect("state file");
        let heartbeat = HeartbeatWriter::new(state_file);
        assert!(matches!(heartbeat.write(), Err(Error::Heartbeat { .. })));
    }
}
