use std::{
    error::Error as StdError,
    fmt,
    time::{Duration, Instant},
};

use bytes::Bytes;
use prelude_mfi::{CERTIFICATE_SERIAL_LEN, CHALLENGE_LEN, MfiAccess, RESPONSE_LEN};
use thiserror::Error;

use crate::csm::{CsmError, CsmFrame, auth::AuthenticationMessage};

pub const CERTIFICATE_RESPONSE_DEADLINE: Duration = Duration::from_secs(1);
pub const CHALLENGE_RESPONSE_DEADLINE: Duration = Duration::from_secs(2);
pub const AUTHENTICATION_FLOW_DEADLINE: Duration = Duration::from_secs(15);

pub trait AuthProvider {
    type Error: StdError + Send + Sync + 'static;

    fn certificate(&mut self) -> Result<Vec<u8>, Self::Error>;
    fn certificate_serial(&mut self) -> Result<[u8; CERTIFICATE_SERIAL_LEN], Self::Error>;
    fn sign(&mut self, challenge: &[u8; CHALLENGE_LEN]) -> Result<[u8; RESPONSE_LEN], Self::Error>;
}

impl<T: MfiAccess> AuthProvider for T {
    type Error = prelude_mfi::Error;

    fn certificate(&mut self) -> Result<Vec<u8>, Self::Error> {
        MfiAccess::certificate(self)
    }

    fn certificate_serial(&mut self) -> Result<[u8; CERTIFICATE_SERIAL_LEN], Self::Error> {
        MfiAccess::certificate_serial(self)
    }

    fn sign(&mut self, challenge: &[u8; CHALLENGE_LEN]) -> Result<[u8; RESPONSE_LEN], Self::Error> {
        MfiAccess::sign(self, challenge)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthState {
    AwaitingCertificateRequest,
    AwaitingCertificateOrChallenge,
    AwaitingChallenge,
    AwaitingResult,
    Authenticated,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthAction {
    Send(CsmFrame),
    Authenticated,
    RejectedByDevice,
}

pub struct AuthMachine<P: AuthProvider> {
    provider: P,
    state: AuthState,
    started_at: Option<Instant>,
}

impl<P: AuthProvider> fmt::Debug for AuthMachine<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthMachine")
            .field("state", &self.state)
            .field("started_at", &self.started_at)
            .finish_non_exhaustive()
    }
}

impl<P: AuthProvider> AuthMachine<P> {
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            state: AuthState::AwaitingCertificateRequest,
            started_at: None,
        }
    }

    pub fn state(&self) -> AuthState {
        self.state
    }

    pub fn provider(&self) -> &P {
        &self.provider
    }

    pub fn provider_mut(&mut self) -> &mut P {
        &mut self.provider
    }

    pub fn into_provider(self) -> P {
        self.provider
    }

    pub fn reset(&mut self) {
        self.state = AuthState::AwaitingCertificateRequest;
        self.started_at = None;
    }

    pub fn deadline(&self) -> Option<Instant> {
        if matches!(self.state, AuthState::Authenticated | AuthState::Failed) {
            None
        } else {
            self.started_at
                .map(|started| started + AUTHENTICATION_FLOW_DEADLINE)
        }
    }

    pub fn check_deadline(&mut self) -> Result<(), AuthError> {
        self.ensure_overall_deadline()
    }

    pub fn handle(&mut self, frame: &CsmFrame) -> Result<AuthAction, AuthError> {
        if matches!(self.state, AuthState::Authenticated | AuthState::Failed) {
            return self.fail(AuthError::TerminalState(self.state));
        }
        let now = Instant::now();
        if self
            .started_at
            .is_some_and(|started| now.duration_since(started) > AUTHENTICATION_FLOW_DEADLINE)
        {
            return self.fail(AuthError::OverallDeadlineExceeded);
        }

        let message = AuthenticationMessage::from_frame(frame).map_err(|error| {
            self.state = AuthState::Failed;
            AuthError::MalformedMessage(error)
        })?;
        match (self.state, message) {
            (
                AuthState::AwaitingCertificateRequest,
                AuthenticationMessage::RequestCertificate {
                    request_serial_number,
                },
            ) => {
                self.started_at = Some(now);
                self.reply_to_certificate_request(request_serial_number, now)
            }
            (
                AuthState::AwaitingCertificateOrChallenge,
                AuthenticationMessage::RequestCertificate {
                    request_serial_number: false,
                },
            ) => self.reply_to_certificate_request(false, now),
            (
                AuthState::AwaitingCertificateOrChallenge | AuthState::AwaitingChallenge,
                AuthenticationMessage::RequestChallengeResponse(challenge),
            ) => {
                let response = self.provider.sign(&challenge).map_err(|error| {
                    self.state = AuthState::Failed;
                    AuthError::Provider(error.to_string())
                })?;
                if now.elapsed() > CHALLENGE_RESPONSE_DEADLINE {
                    return self.fail(AuthError::ChallengeDeadlineExceeded);
                }
                self.ensure_overall_deadline()?;
                self.state = AuthState::AwaitingResult;
                Ok(AuthAction::Send(
                    AuthenticationMessage::Response(response).to_frame(),
                ))
            }
            (AuthState::AwaitingResult, AuthenticationMessage::Succeeded) => {
                self.ensure_overall_deadline()?;
                self.state = AuthState::Authenticated;
                Ok(AuthAction::Authenticated)
            }
            (AuthState::AwaitingResult, AuthenticationMessage::Failed) => {
                self.state = AuthState::Failed;
                Ok(AuthAction::RejectedByDevice)
            }
            (state, message) => self.fail(AuthError::OutOfOrder {
                state,
                message: message_name(&message),
            }),
        }
    }

    fn reply_to_certificate_request(
        &mut self,
        request_serial_number: bool,
        received_at: Instant,
    ) -> Result<AuthAction, AuthError> {
        let message = if request_serial_number {
            let serial = self.provider.certificate_serial().map_err(|error| {
                self.state = AuthState::Failed;
                AuthError::Provider(error.to_string())
            })?;
            self.state = AuthState::AwaitingCertificateOrChallenge;
            AuthenticationMessage::AccessorySerialNumber(serial)
        } else {
            let certificate = self.provider.certificate().map_err(|error| {
                self.state = AuthState::Failed;
                AuthError::Provider(error.to_string())
            })?;
            self.state = AuthState::AwaitingChallenge;
            AuthenticationMessage::Certificate(Bytes::from(certificate))
        };
        if received_at.elapsed() > CERTIFICATE_RESPONSE_DEADLINE {
            return self.fail(AuthError::CertificateDeadlineExceeded);
        }
        self.ensure_overall_deadline()?;
        Ok(AuthAction::Send(message.to_frame()))
    }

    fn ensure_overall_deadline(&mut self) -> Result<(), AuthError> {
        if self
            .started_at
            .is_some_and(|started| started.elapsed() > AUTHENTICATION_FLOW_DEADLINE)
        {
            return self.fail(AuthError::OverallDeadlineExceeded);
        }
        Ok(())
    }

    fn fail<T>(&mut self, error: AuthError) -> Result<T, AuthError> {
        self.state = AuthState::Failed;
        Err(error)
    }
}

fn message_name(message: &AuthenticationMessage) -> &'static str {
    match message {
        AuthenticationMessage::RequestCertificate { .. } => "RequestAuthenticationCertificate",
        AuthenticationMessage::Certificate(_) => "AuthenticationCertificate",
        AuthenticationMessage::RequestChallengeResponse(_) => {
            "RequestAuthenticationChallengeResponse"
        }
        AuthenticationMessage::Response(_) => "AuthenticationResponse",
        AuthenticationMessage::Failed => "AuthenticationFailed",
        AuthenticationMessage::Succeeded => "AuthenticationSucceeded",
        AuthenticationMessage::AccessorySerialNumber(_) => "AccessoryAuthenticationSerialNumber",
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AuthError {
    #[error("malformed iAP2 authentication message: {0}")]
    MalformedMessage(CsmError),
    #[error("authentication provider failed: {0}")]
    Provider(String),
    #[error("authentication message {message} is invalid while in state {state:?}")]
    OutOfOrder {
        state: AuthState,
        message: &'static str,
    },
    #[error("authentication state {0:?} is terminal")]
    TerminalState(AuthState),
    #[error("certificate response exceeded the one-second deadline")]
    CertificateDeadlineExceeded,
    #[error("challenge response exceeded the two-second deadline")]
    ChallengeDeadlineExceeded,
    #[error("authentication exceeded the fifteen-second overall deadline")]
    OverallDeadlineExceeded,
}

#[cfg(test)]
mod tests {
    use std::{io, thread};

    use super::*;
    use crate::csm::auth::{
        ACCESSORY_AUTHENTICATION_SERIAL_NUMBER, AUTHENTICATION_CERTIFICATE, AUTHENTICATION_RESPONSE,
    };

    #[derive(Debug, Clone)]
    struct FakeProvider {
        fail: bool,
        certificate_delay: Duration,
        challenge_delay: Duration,
    }

    impl Default for FakeProvider {
        fn default() -> Self {
            Self {
                fail: false,
                certificate_delay: Duration::ZERO,
                challenge_delay: Duration::ZERO,
            }
        }
    }

    impl AuthProvider for FakeProvider {
        type Error = io::Error;

        fn certificate(&mut self) -> Result<Vec<u8>, Self::Error> {
            thread::sleep(self.certificate_delay);
            if self.fail {
                Err(io::Error::other("injected provider failure"))
            } else {
                Ok(vec![0x30; 608])
            }
        }

        fn certificate_serial(&mut self) -> Result<[u8; CERTIFICATE_SERIAL_LEN], Self::Error> {
            if self.fail {
                Err(io::Error::other("injected provider failure"))
            } else {
                Ok([0x12; CERTIFICATE_SERIAL_LEN])
            }
        }

        fn sign(
            &mut self,
            _challenge: &[u8; CHALLENGE_LEN],
        ) -> Result<[u8; RESPONSE_LEN], Self::Error> {
            thread::sleep(self.challenge_delay);
            if self.fail {
                Err(io::Error::other("injected provider failure"))
            } else {
                Ok([0x34; RESPONSE_LEN])
            }
        }
    }

    #[test]
    fn complete_certificate_challenge_success_flow() {
        let mut machine = AuthMachine::new(FakeProvider::default());
        let request = AuthenticationMessage::RequestCertificate {
            request_serial_number: false,
        }
        .to_frame();
        let AuthAction::Send(certificate) = machine.handle(&request).expect("certificate reply")
        else {
            panic!("expected certificate reply");
        };
        assert_eq!(certificate.message_id, AUTHENTICATION_CERTIFICATE);
        assert_eq!(machine.state(), AuthState::AwaitingChallenge);

        let challenge =
            AuthenticationMessage::RequestChallengeResponse([0x56; CHALLENGE_LEN]).to_frame();
        let AuthAction::Send(response) = machine.handle(&challenge).expect("challenge reply")
        else {
            panic!("expected challenge reply");
        };
        assert_eq!(response.message_id, AUTHENTICATION_RESPONSE);
        assert_eq!(machine.state(), AuthState::AwaitingResult);

        let result = machine
            .handle(&AuthenticationMessage::Succeeded.to_frame())
            .expect("success");
        assert_eq!(result, AuthAction::Authenticated);
        assert_eq!(machine.state(), AuthState::Authenticated);
    }

    #[test]
    fn serial_cache_probe_can_continue_directly_to_challenge() {
        let mut machine = AuthMachine::new(FakeProvider::default());
        let request = AuthenticationMessage::RequestCertificate {
            request_serial_number: true,
        }
        .to_frame();
        let AuthAction::Send(serial) = machine.handle(&request).expect("serial reply") else {
            panic!("expected serial reply");
        };
        assert_eq!(serial.message_id, ACCESSORY_AUTHENTICATION_SERIAL_NUMBER);
        assert_eq!(machine.state(), AuthState::AwaitingCertificateOrChallenge);

        let challenge =
            AuthenticationMessage::RequestChallengeResponse([0x56; CHALLENGE_LEN]).to_frame();
        assert!(matches!(
            machine.handle(&challenge),
            Ok(AuthAction::Send(_))
        ));
    }

    #[test]
    fn serial_cache_miss_can_request_full_certificate() {
        let mut machine = AuthMachine::new(FakeProvider::default());
        machine
            .handle(
                &AuthenticationMessage::RequestCertificate {
                    request_serial_number: true,
                }
                .to_frame(),
            )
            .expect("serial reply");
        let result = machine
            .handle(
                &AuthenticationMessage::RequestCertificate {
                    request_serial_number: false,
                }
                .to_frame(),
            )
            .expect("certificate reply");
        assert!(matches!(result, AuthAction::Send(_)));
        assert_eq!(machine.state(), AuthState::AwaitingChallenge);
    }

    #[test]
    fn challenge_before_certificate_request_fails_closed() {
        let mut machine = AuthMachine::new(FakeProvider::default());
        let error = machine
            .handle(&AuthenticationMessage::RequestChallengeResponse([0; CHALLENGE_LEN]).to_frame())
            .expect_err("out-of-order challenge");
        assert!(matches!(error, AuthError::OutOfOrder { .. }));
        assert_eq!(machine.state(), AuthState::Failed);
    }

    #[test]
    fn malformed_message_fails_closed() {
        let mut machine = AuthMachine::new(FakeProvider::default());
        let malformed = CsmFrame {
            message_id: crate::csm::auth::REQUEST_AUTHENTICATION_CERTIFICATE,
            parameters: vec![crate::csm::CsmParameter::new(0, Bytes::from_static(&[1]))],
        };
        assert!(matches!(
            machine.handle(&malformed),
            Err(AuthError::MalformedMessage(_))
        ));
        assert_eq!(machine.state(), AuthState::Failed);
    }

    #[test]
    fn provider_error_fails_closed_without_credential_fallback() {
        let mut machine = AuthMachine::new(FakeProvider {
            fail: true,
            ..FakeProvider::default()
        });
        assert!(matches!(
            machine.handle(
                &AuthenticationMessage::RequestCertificate {
                    request_serial_number: false,
                }
                .to_frame()
            ),
            Err(AuthError::Provider(_))
        ));
        assert_eq!(machine.state(), AuthState::Failed);
    }

    #[test]
    fn overall_deadline_is_enforced() {
        let mut machine = AuthMachine::new(FakeProvider::default());
        machine.state = AuthState::AwaitingChallenge;
        machine.started_at = Some(Instant::now() - Duration::from_secs(16));
        assert_eq!(
            machine
                .handle(
                    &AuthenticationMessage::RequestChallengeResponse([0; CHALLENGE_LEN]).to_frame()
                )
                .expect_err("expired flow"),
            AuthError::OverallDeadlineExceeded
        );
        assert_eq!(machine.state(), AuthState::Failed);
    }

    #[test]
    fn certificate_deadline_is_enforced() {
        let mut machine = AuthMachine::new(FakeProvider {
            certificate_delay: Duration::from_millis(1_010),
            ..FakeProvider::default()
        });
        assert_eq!(
            machine
                .handle(
                    &AuthenticationMessage::RequestCertificate {
                        request_serial_number: false,
                    }
                    .to_frame()
                )
                .expect_err("slow certificate"),
            AuthError::CertificateDeadlineExceeded
        );
        assert_eq!(machine.state(), AuthState::Failed);
    }

    #[test]
    fn challenge_deadline_is_enforced() {
        let mut machine = AuthMachine::new(FakeProvider {
            challenge_delay: Duration::from_millis(2_010),
            ..FakeProvider::default()
        });
        machine
            .handle(
                &AuthenticationMessage::RequestCertificate {
                    request_serial_number: false,
                }
                .to_frame(),
            )
            .expect("certificate reply");
        assert_eq!(
            machine
                .handle(
                    &AuthenticationMessage::RequestChallengeResponse([0; CHALLENGE_LEN]).to_frame()
                )
                .expect_err("slow challenge response"),
            AuthError::ChallengeDeadlineExceeded
        );
        assert_eq!(machine.state(), AuthState::Failed);
    }
}
