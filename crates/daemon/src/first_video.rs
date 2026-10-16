use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FirstVideoAction {
    Wait,
    RequestKeyFrame,
    Expired,
}

#[derive(Debug)]
pub(crate) struct FirstVideoWatchdog {
    started_at: Instant,
    requested_keyframe: bool,
}

impl FirstVideoWatchdog {
    pub(crate) const KEYFRAME_AFTER: Duration = Duration::from_secs(5);
    pub(crate) const TIMEOUT: Duration = Duration::from_secs(15);

    pub(crate) const fn new(started_at: Instant) -> Self {
        Self {
            started_at,
            requested_keyframe: false,
        }
    }

    pub(crate) fn check(&mut self, now: Instant) -> FirstVideoAction {
        let waited = now.saturating_duration_since(self.started_at);
        if waited >= Self::TIMEOUT {
            return FirstVideoAction::Expired;
        }
        if waited >= Self::KEYFRAME_AFTER && !self.requested_keyframe {
            self.requested_keyframe = true;
            return FirstVideoAction::RequestKeyFrame;
        }
        FirstVideoAction::Wait
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_one_keyframe_then_expires() {
        let started = Instant::now();
        let mut watchdog = FirstVideoWatchdog::new(started);
        assert_eq!(
            watchdog.check(started + Duration::from_secs(4)),
            FirstVideoAction::Wait
        );
        assert_eq!(
            watchdog.check(started + FirstVideoWatchdog::KEYFRAME_AFTER),
            FirstVideoAction::RequestKeyFrame
        );
        assert_eq!(
            watchdog.check(started + Duration::from_secs(10)),
            FirstVideoAction::Wait
        );
        assert_eq!(
            watchdog.check(started + FirstVideoWatchdog::TIMEOUT),
            FirstVideoAction::Expired
        );
    }

    #[test]
    fn expires_without_a_late_keyframe_request() {
        let started = Instant::now();
        let mut watchdog = FirstVideoWatchdog::new(started);
        assert_eq!(
            watchdog.check(started + Duration::from_secs(20)),
            FirstVideoAction::Expired
        );
    }
}
