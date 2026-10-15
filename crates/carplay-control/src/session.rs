use prelude_net::SecurityContext;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImplementationStatus {
    Unsupported,
    AndroidAuto,
    ExperimentalPrivateProtocol,
    AppleLicensedPlugin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProjectionCapabilities {
    pub control: bool,
    pub video: bool,
    pub audio: bool,
    pub microphone: bool,
    pub input: bool,
}

impl ProjectionCapabilities {
    pub const fn complete(self) -> bool {
        self.control && self.video && self.audio && self.microphone && self.input
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProjectionPolicy {
    pub allow_experimental_private_protocol: bool,
    pub licensed_plugin_loaded: bool,
    pub require_complete_capabilities: bool,
}

#[derive(Debug)]
pub struct ProjectionAuthorization {
    implementation: ImplementationStatus,
    capabilities: ProjectionCapabilities,
}

impl ProjectionAuthorization {
    pub fn implementation(&self) -> ImplementationStatus {
        self.implementation
    }

    pub fn capabilities(&self) -> ProjectionCapabilities {
        self.capabilities
    }
}

#[derive(Debug, Default)]
pub struct ProjectionGate;

impl ProjectionGate {
    pub fn authorize(
        &self,
        security: &SecurityContext,
        implementation: ImplementationStatus,
        capabilities: ProjectionCapabilities,
        policy: ProjectionPolicy,
    ) -> Result<ProjectionAuthorization> {
        match implementation {
            ImplementationStatus::Unsupported => {
                return Err(Error::Unsupported(
                    "no projection protocol implementation is available",
                ));
            }
            ImplementationStatus::AndroidAuto => {
                return Err(Error::Unsupported(
                    "Android Auto authentication is owned by its AAP runtime, not the CarPlay security gate",
                ));
            }
            ImplementationStatus::ExperimentalPrivateProtocol
            | ImplementationStatus::AppleLicensedPlugin => {}
        }
        security.require_ready().map_err(|_| Error::NotReady {
            component: "projection security",
            state: security.state().as_str(),
        })?;
        match implementation {
            ImplementationStatus::ExperimentalPrivateProtocol
                if !policy.allow_experimental_private_protocol =>
            {
                return Err(Error::Unsupported(
                    "experimental private projection protocol is disabled by policy",
                ));
            }
            ImplementationStatus::AppleLicensedPlugin if !policy.licensed_plugin_loaded => {
                return Err(Error::NotReady {
                    component: "licensed CarPlay plug-in",
                    state: "not_loaded",
                });
            }
            ImplementationStatus::Unsupported
            | ImplementationStatus::AndroidAuto
            | ImplementationStatus::ExperimentalPrivateProtocol
            | ImplementationStatus::AppleLicensedPlugin => {}
        }
        if !capabilities.control {
            return Err(Error::NotReady {
                component: "projection capabilities",
                state: "control_unavailable",
            });
        }
        if policy.require_complete_capabilities && !capabilities.complete() {
            return Err(Error::NotReady {
                component: "projection capabilities",
                state: "incomplete",
            });
        }
        Ok(ProjectionAuthorization {
            implementation,
            capabilities,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionState {
    Disconnected,
    TransportReady,
    Authorized,
    Active,
    Stopped,
    Failed,
}

impl ProjectionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::TransportReady => "transport_ready",
            Self::Authorized => "authorized",
            Self::Active => "active",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionEvent {
    TransportConnected,
    Authorized,
    Start,
    Stop,
    Fatal,
}

#[derive(Debug)]
pub struct ProjectionSession {
    state: ProjectionState,
    authorization: Option<ProjectionAuthorization>,
}

impl Default for ProjectionSession {
    fn default() -> Self {
        Self {
            state: ProjectionState::Disconnected,
            authorization: None,
        }
    }
}

impl ProjectionSession {
    pub fn state(&self) -> ProjectionState {
        self.state
    }

    pub fn authorize(&mut self, authorization: ProjectionAuthorization) -> Result<()> {
        if self.state != ProjectionState::TransportReady {
            return Err(self.not_ready());
        }
        self.authorization = Some(authorization);
        self.state = ProjectionState::Authorized;
        Ok(())
    }

    pub fn authorization(&self) -> Result<&ProjectionAuthorization> {
        if !matches!(
            self.state,
            ProjectionState::Authorized | ProjectionState::Active
        ) {
            return Err(self.not_ready());
        }
        self.authorization.as_ref().ok_or_else(|| self.not_ready())
    }

    pub fn apply(&mut self, event: ProjectionEvent) -> Result<ProjectionState> {
        if event == ProjectionEvent::Fatal {
            self.authorization = None;
            self.state = ProjectionState::Failed;
            return Ok(self.state);
        }
        if event == ProjectionEvent::Stop {
            self.authorization = None;
            self.state = ProjectionState::Stopped;
            return Ok(self.state);
        }
        let next = match (self.state, event) {
            (ProjectionState::Disconnected, ProjectionEvent::TransportConnected) => {
                ProjectionState::TransportReady
            }
            (ProjectionState::Authorized, ProjectionEvent::Start)
                if self.authorization.is_some() =>
            {
                ProjectionState::Active
            }
            (state, _) => {
                return Err(Error::NotReady {
                    component: "projection session",
                    state: state.as_str(),
                });
            }
        };
        self.state = next;
        Ok(next)
    }

    fn not_ready(&self) -> Error {
        Error::NotReady {
            component: "projection session",
            state: self.state.as_str(),
        }
    }
}

#[cfg(test)]
mod tests {
    use prelude_net::PairVerification;

    use super::*;

    fn ready_security() -> SecurityContext {
        let mut security = SecurityContext::default();
        security
            .record_pair_verification(PairVerification::new("phone", [1; 32]).expect("pairing"))
            .expect("pairing");
        security.record_mfi_proof().expect("proof");
        security.record_peer_acceptance().expect("acceptance");
        security
    }

    #[test]
    fn unsupported_implementation_cannot_be_authorized() {
        let result = ProjectionGate.authorize(
            &ready_security(),
            ImplementationStatus::Unsupported,
            ProjectionCapabilities {
                control: true,
                ..ProjectionCapabilities::default()
            },
            ProjectionPolicy::default(),
        );
        assert!(matches!(result, Err(Error::Unsupported(_))));
    }

    #[test]
    fn android_auto_never_enters_the_carplay_security_gate() {
        let result = ProjectionGate.authorize(
            &SecurityContext::default(),
            ImplementationStatus::AndroidAuto,
            ProjectionCapabilities {
                control: true,
                ..ProjectionCapabilities::default()
            },
            ProjectionPolicy::default(),
        );
        assert!(matches!(result, Err(Error::Unsupported(_))));
    }

    #[test]
    fn experimental_implementation_requires_explicit_policy() {
        let security = ready_security();
        let capabilities = ProjectionCapabilities {
            control: true,
            ..ProjectionCapabilities::default()
        };
        assert!(matches!(
            ProjectionGate.authorize(
                &security,
                ImplementationStatus::ExperimentalPrivateProtocol,
                capabilities,
                ProjectionPolicy::default(),
            ),
            Err(Error::Unsupported(_))
        ));
        assert!(
            ProjectionGate
                .authorize(
                    &security,
                    ImplementationStatus::ExperimentalPrivateProtocol,
                    capabilities,
                    ProjectionPolicy {
                        allow_experimental_private_protocol: true,
                        ..ProjectionPolicy::default()
                    },
                )
                .is_ok()
        );
    }

    #[test]
    fn session_cannot_start_without_authorization() {
        let mut session = ProjectionSession::default();
        session
            .apply(ProjectionEvent::TransportConnected)
            .expect("transport");
        assert!(matches!(
            session.apply(ProjectionEvent::Start),
            Err(Error::NotReady { .. })
        ));
    }
}
