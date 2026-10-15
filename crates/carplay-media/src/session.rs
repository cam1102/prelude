use prelude_control::ProjectionAuthorization;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaState {
    Idle,
    Authorized,
    Running,
    Draining,
    Stopped,
    Failed,
}

impl MediaState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Authorized => "authorized",
            Self::Running => "running",
            Self::Draining => "draining",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaEvent {
    Start,
    Drain,
    Stop,
    Fatal,
}

#[derive(Debug)]
pub struct MediaSession {
    state: MediaState,
    video_enabled: bool,
    audio_enabled: bool,
    microphone_enabled: bool,
}

impl Default for MediaSession {
    fn default() -> Self {
        Self {
            state: MediaState::Idle,
            video_enabled: false,
            audio_enabled: false,
            microphone_enabled: false,
        }
    }
}

impl MediaSession {
    pub fn authorize(&mut self, authorization: &ProjectionAuthorization) -> Result<()> {
        if self.state != MediaState::Idle {
            return Err(self.not_ready());
        }
        let capabilities = authorization.capabilities();
        if !capabilities.video && !capabilities.audio {
            return Err(Error::Unsupported(
                "projection negotiated neither video nor audio",
            ));
        }
        self.video_enabled = capabilities.video;
        self.audio_enabled = capabilities.audio;
        self.microphone_enabled = capabilities.microphone;
        self.state = MediaState::Authorized;
        Ok(())
    }

    pub fn state(&self) -> MediaState {
        self.state
    }

    pub fn require_video(&self) -> Result<()> {
        self.require_running()?;
        if !self.video_enabled {
            return Err(Error::Unsupported("video was not negotiated"));
        }
        Ok(())
    }

    pub fn require_audio(&self) -> Result<()> {
        self.require_running()?;
        if !self.audio_enabled {
            return Err(Error::Unsupported("audio was not negotiated"));
        }
        Ok(())
    }

    pub fn require_microphone(&self) -> Result<()> {
        self.require_running()?;
        if !self.microphone_enabled {
            return Err(Error::Unsupported("microphone uplink was not negotiated"));
        }
        Ok(())
    }

    pub fn apply(&mut self, event: MediaEvent) -> Result<MediaState> {
        if event == MediaEvent::Fatal {
            self.state = MediaState::Failed;
            return Ok(self.state);
        }
        if event == MediaEvent::Stop {
            self.state = MediaState::Stopped;
            return Ok(self.state);
        }
        let next = match (self.state, event) {
            (MediaState::Authorized, MediaEvent::Start) => MediaState::Running,
            (MediaState::Running, MediaEvent::Drain) => MediaState::Draining,
            (state, _) => {
                return Err(Error::NotReady {
                    component: "media session",
                    state: state.as_str(),
                });
            }
        };
        self.state = next;
        Ok(next)
    }

    fn require_running(&self) -> Result<()> {
        if self.state != MediaState::Running {
            return Err(self.not_ready());
        }
        Ok(())
    }

    fn not_ready(&self) -> Error {
        Error::NotReady {
            component: "media session",
            state: self.state.as_str(),
        }
    }
}

#[cfg(test)]
mod tests {
    use prelude_control::{
        ImplementationStatus, ProjectionCapabilities, ProjectionGate, ProjectionPolicy,
    };
    use prelude_net::{PairVerification, SecurityContext};

    use super::*;

    fn authorization() -> prelude_control::ProjectionAuthorization {
        let mut security = SecurityContext::default();
        security
            .record_pair_verification(PairVerification::new("phone", [4; 32]).expect("pairing"))
            .expect("pairing");
        security.record_mfi_proof().expect("proof");
        security.record_peer_acceptance().expect("acceptance");
        ProjectionGate
            .authorize(
                &security,
                ImplementationStatus::ExperimentalPrivateProtocol,
                ProjectionCapabilities {
                    control: true,
                    video: true,
                    audio: true,
                    ..ProjectionCapabilities::default()
                },
                ProjectionPolicy {
                    allow_experimental_private_protocol: true,
                    ..ProjectionPolicy::default()
                },
            )
            .expect("authorization")
    }

    #[test]
    fn media_requires_authorization_then_start() {
        let mut session = MediaSession::default();
        assert!(matches!(
            session.apply(MediaEvent::Start),
            Err(Error::NotReady { .. })
        ));
        session.authorize(&authorization()).expect("authorization");
        assert!(matches!(
            session.require_video(),
            Err(Error::NotReady { .. })
        ));
        session.apply(MediaEvent::Start).expect("start");
        session.require_video().expect("video");
        assert!(matches!(
            session.require_microphone(),
            Err(Error::Unsupported(_))
        ));
    }
}
