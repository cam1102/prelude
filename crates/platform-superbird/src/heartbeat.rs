use std::time::Duration;

#[cfg(target_os = "linux")]
use std::fs;

use nix::time::{ClockId, clock_gettime};
use thiserror::Error;

const VERSION: &str = "version=1";
#[cfg(target_os = "linux")]
const LINUX_BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatStamp {
    boot_id: String,
    boottime_millis: u64,
}

#[derive(Debug, Error)]
pub enum HeartbeatStampError {
    #[error("heartbeat clock failed: {0}")]
    Clock(String),
    #[error("heartbeat boot identity could not be read: {0}")]
    BootIdentity(#[source] std::io::Error),
    #[error("heartbeat stamp is malformed or noncanonical")]
    Malformed,
    #[error("heartbeat belongs to a different boot")]
    DifferentBoot,
    #[error("heartbeat monotonic timestamp is in the future")]
    Future,
}

impl HeartbeatStamp {
    pub fn current() -> Result<Self, HeartbeatStampError> {
        #[cfg(target_os = "linux")]
        let boot_id = fs::read_to_string(LINUX_BOOT_ID)
            .map_err(HeartbeatStampError::BootIdentity)?
            .trim()
            .to_owned();
        #[cfg(not(target_os = "linux"))]
        let boot_id = "non-linux-test-host".to_owned();

        validate_boot_id(&boot_id)?;
        #[cfg(target_os = "linux")]
        let clock = ClockId::CLOCK_BOOTTIME;
        #[cfg(not(target_os = "linux"))]
        let clock = ClockId::CLOCK_MONOTONIC;
        let time =
            clock_gettime(clock).map_err(|error| HeartbeatStampError::Clock(error.to_string()))?;
        let seconds = u64::try_from(time.tv_sec()).map_err(|_| HeartbeatStampError::Future)?;
        let nanos = u64::try_from(time.tv_nsec()).map_err(|_| HeartbeatStampError::Future)?;
        let boottime_millis = seconds
            .checked_mul(1_000)
            .and_then(|value| value.checked_add(nanos / 1_000_000))
            .ok_or(HeartbeatStampError::Future)?;
        Ok(Self {
            boot_id,
            boottime_millis,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        format!(
            "{VERSION}\nboot_id={}\nboottime_millis={}\n",
            self.boot_id, self.boottime_millis
        )
        .into_bytes()
    }

    pub fn decode(contents: &[u8]) -> Result<Self, HeartbeatStampError> {
        let text = std::str::from_utf8(contents).map_err(|_| HeartbeatStampError::Malformed)?;
        let mut lines = text.lines();
        if lines.next() != Some(VERSION) {
            return Err(HeartbeatStampError::Malformed);
        }
        let boot_id = lines
            .next()
            .and_then(|line| line.strip_prefix("boot_id="))
            .ok_or(HeartbeatStampError::Malformed)?
            .to_owned();
        validate_boot_id(&boot_id)?;
        let boottime_millis = lines
            .next()
            .and_then(|line| line.strip_prefix("boottime_millis="))
            .ok_or(HeartbeatStampError::Malformed)?
            .parse::<u64>()
            .map_err(|_| HeartbeatStampError::Malformed)?;
        let stamp = Self {
            boot_id,
            boottime_millis,
        };
        if lines.next().is_some() || stamp.encode() != contents {
            return Err(HeartbeatStampError::Malformed);
        }
        Ok(stamp)
    }

    pub fn age_at(&self, current: &Self) -> Result<Duration, HeartbeatStampError> {
        if self.boot_id != current.boot_id {
            return Err(HeartbeatStampError::DifferentBoot);
        }
        let millis = current
            .boottime_millis
            .checked_sub(self.boottime_millis)
            .ok_or(HeartbeatStampError::Future)?;
        Ok(Duration::from_millis(millis))
    }

    #[cfg(test)]
    fn from_parts(boot_id: &str, boottime_millis: u64) -> Self {
        Self {
            boot_id: boot_id.to_owned(),
            boottime_millis,
        }
    }
}

fn validate_boot_id(boot_id: &str) -> Result<(), HeartbeatStampError> {
    if boot_id.is_empty()
        || boot_id.len() > 64
        || !boot_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(HeartbeatStampError::Malformed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_round_trip_and_monotonic_age() {
        let stamp = HeartbeatStamp::from_parts("boot-a", 1_500);
        assert_eq!(
            HeartbeatStamp::decode(&stamp.encode()).expect("decode"),
            stamp
        );
        assert_eq!(
            stamp
                .age_at(&HeartbeatStamp::from_parts("boot-a", 2_750))
                .expect("age"),
            Duration::from_millis(1_250)
        );
    }

    #[test]
    fn rejects_stale_boot_future_and_noncanonical_inputs() {
        let stamp = HeartbeatStamp::from_parts("boot-a", 1_500);
        assert!(matches!(
            stamp.age_at(&HeartbeatStamp::from_parts("boot-b", 2_000)),
            Err(HeartbeatStampError::DifferentBoot)
        ));
        assert!(matches!(
            stamp.age_at(&HeartbeatStamp::from_parts("boot-a", 1_000)),
            Err(HeartbeatStampError::Future)
        ));
        for malformed in [
            b"version=1\nboot_id=bad id\nboottime_millis=1\n".as_slice(),
            b"version=1\nboot_id=boot-a\nboottime_millis=01\n".as_slice(),
            b"version=1\nboot_id=boot-a\nboottime_millis=1".as_slice(),
        ] {
            assert!(matches!(
                HeartbeatStamp::decode(malformed),
                Err(HeartbeatStampError::Malformed)
            ));
        }
    }
}
