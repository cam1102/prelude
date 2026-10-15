use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::{ControlKeys, Error, EventChannelKeys, Result, crypto::hkdf_sha512};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataStreamDirection {
    Output,
    Input,
}

pub struct PairVerification {
    controller_id: String,
    shared_secret: Zeroizing<[u8; 32]>,
}

impl std::fmt::Debug for PairVerification {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PairVerification")
            .field("controller_id", &self.controller_id)
            .field("shared_secret", &"[redacted]")
            .finish()
    }
}

impl PairVerification {
    pub fn new(controller_id: impl Into<String>, shared_secret: [u8; 32]) -> Result<Self> {
        let controller_id = controller_id.into();
        if controller_id.trim().is_empty()
            || controller_id.len() > 256
            || controller_id.chars().any(char::is_control)
        {
            return Err(Error::Crypto("pair-verify controller identifier is empty"));
        }
        if bool::from(shared_secret.ct_eq(&[0; 32])) {
            return Err(Error::Crypto("pair-verify shared secret is all zero"));
        }
        Ok(Self {
            controller_id,
            shared_secret: Zeroizing::new(shared_secret),
        })
    }

    pub fn controller_id(&self) -> &str {
        &self.controller_id
    }

    pub fn derive_key(&self, salt: &[u8], info: &[u8], output: &mut [u8]) -> Result<()> {
        hkdf_sha512(self.shared_secret.as_ref(), salt, info, output)
    }

    pub fn control_keys(&self) -> Result<ControlKeys> {
        let mut read_key = [0; 32];
        let mut write_key = [0; 32];
        self.derive_key(
            b"Control-Salt",
            b"Control-Write-Encryption-Key",
            &mut read_key,
        )?;
        self.derive_key(
            b"Control-Salt",
            b"Control-Read-Encryption-Key",
            &mut write_key,
        )?;
        ControlKeys::new(read_key, write_key)
    }

    pub fn event_channel_keys(&self) -> Result<EventChannelKeys> {
        let mut read_key = [0; 32];
        let mut write_key = [0; 32];
        self.derive_key(b"Events-Salt", b"Events-Read-Encryption-Key", &mut read_key)?;
        self.derive_key(
            b"Events-Salt",
            b"Events-Write-Encryption-Key",
            &mut write_key,
        )?;
        EventChannelKeys::new(read_key, write_key)
    }

    pub fn data_stream_key(
        &self,
        connection_id: u64,
        direction: DataStreamDirection,
    ) -> Result<Zeroizing<[u8; 32]>> {
        let salt = format!("DataStream-Salt{connection_id}");
        let info: &[u8] = match direction {
            DataStreamDirection::Output => b"DataStream-Output-Encryption-Key",
            DataStreamDirection::Input => b"DataStream-Input-Encryption-Key",
        };
        let mut key = Zeroizing::new([0; 32]);
        self.derive_key(salt.as_bytes(), info, key.as_mut())?;
        Ok(key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecurityState {
    Cleartext,
    PairVerified,
    MfiProofIssued,
    Ready,
    Failed,
}

impl SecurityState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cleartext => "cleartext",
            Self::PairVerified => "pair_verified",
            Self::MfiProofIssued => "mfi_proof_issued",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug)]
pub struct SecurityContext {
    state: SecurityState,
    pairing: Option<PairVerification>,
}

impl Default for SecurityContext {
    fn default() -> Self {
        Self {
            state: SecurityState::Cleartext,
            pairing: None,
        }
    }
}

impl SecurityContext {
    pub fn state(&self) -> SecurityState {
        self.state
    }

    pub fn record_pair_verification(&mut self, pairing: PairVerification) -> Result<()> {
        if self.state != SecurityState::Cleartext {
            return Err(self.not_ready());
        }
        self.pairing = Some(pairing);
        self.state = SecurityState::PairVerified;
        Ok(())
    }

    pub fn pairing(&self) -> Result<&PairVerification> {
        self.pairing.as_ref().ok_or_else(|| self.not_ready())
    }

    pub fn record_mfi_proof(&mut self) -> Result<()> {
        if self.state != SecurityState::PairVerified {
            return Err(self.not_ready());
        }
        self.state = SecurityState::MfiProofIssued;
        Ok(())
    }

    pub fn record_peer_acceptance(&mut self) -> Result<()> {
        if self.state != SecurityState::MfiProofIssued {
            return Err(self.not_ready());
        }
        self.state = SecurityState::Ready;
        Ok(())
    }

    pub fn require_ready(&self) -> Result<&PairVerification> {
        if self.state != SecurityState::Ready {
            return Err(self.not_ready());
        }
        self.pairing.as_ref().ok_or(Error::Crypto(
            "ready security context lost its pair-verification result",
        ))
    }

    pub fn fail(&mut self) {
        self.state = SecurityState::Failed;
        self.pairing = None;
    }

    fn not_ready(&self) -> Error {
        Error::NotReady {
            component: "control-channel security",
            state: self.state.as_str(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_requires_every_security_gate() {
        let mut context = SecurityContext::default();
        assert!(matches!(
            context.require_ready(),
            Err(Error::NotReady { .. })
        ));
        context
            .record_pair_verification(PairVerification::new("phone", [7; 32]).expect("pairing"))
            .expect("record pairing");
        assert!(matches!(
            context.require_ready(),
            Err(Error::NotReady { .. })
        ));
        context.record_mfi_proof().expect("record proof");
        assert!(matches!(
            context.require_ready(),
            Err(Error::NotReady { .. })
        ));
        context.record_peer_acceptance().expect("record acceptance");
        assert_eq!(
            context.require_ready().expect("ready").controller_id(),
            "phone"
        );
    }

    #[test]
    fn event_key_derivation_matches_pinned_livi_direction_golden() {
        let verification = PairVerification::new("phone", [7; 32]).expect("pairing");
        let keys = verification.event_channel_keys().expect("event keys");
        let mut accessory = crate::EventChannelCipher::new(keys).expect("accessory");
        let expected_read: [u8; 32] =
            hex::decode("8339b0b1d1db821cd53ca43a7005f604107f5b58b75ecf834eed5110e64843a8")
                .expect("hex")
                .try_into()
                .expect("read key");
        let expected_write: [u8; 32] =
            hex::decode("99e56d822e302fb433aab762c4d0f99a502dd0069eb3cd257820b021ec785a2d")
                .expect("hex")
                .try_into()
                .expect("write key");
        let mut phone = crate::EventChannelCipher::new(
            EventChannelKeys::new(expected_write, expected_read).expect("peer keys"),
        )
        .expect("phone");
        let wire = accessory.encrypt(b"first byte encrypted").expect("encrypt");
        assert_eq!(
            hex::encode(&wire),
            "140025cfe120717bfa273b2c1ab17dad1ed09f96b80a953892e6f3a8d86767864bf4db415fe2"
        );
        assert_eq!(
            phone.decrypt(&wire).expect("decrypt"),
            b"first byte encrypted"
        );
    }

    #[test]
    fn data_stream_keys_match_independent_decimal_salt_golden() {
        let verification = PairVerification::new("phone", [7; 32]).expect("pairing");
        let output = verification
            .data_stream_key(123_456_789, DataStreamDirection::Output)
            .expect("output key");
        let input = verification
            .data_stream_key(123_456_789, DataStreamDirection::Input)
            .expect("input key");
        assert_eq!(
            hex::encode(output.as_ref()),
            "c6d42557068dc255546ba0cd17fd8ac357c1f0d6e5e7e5391d18395cd297c3f9"
        );
        assert_eq!(
            hex::encode(input.as_ref()),
            "a27023575ed7f839d33b8365eec96feb634ffdf61ec767e5e500a6a40ab72d9b"
        );
        assert_ne!(output.as_ref(), input.as_ref());
    }

    #[test]
    fn all_zero_pairing_secret_is_rejected() {
        assert!(matches!(
            PairVerification::new("phone", [0; 32]),
            Err(Error::Crypto(_))
        ));
    }

    #[test]
    fn key_derivation_is_deterministic_and_context_separated() {
        let pairing = PairVerification::new("phone", [9; 32]).expect("pairing");
        let mut first = [0; 32];
        let mut second = [0; 32];
        pairing
            .derive_key(b"salt", b"screen", &mut first)
            .expect("key");
        pairing
            .derive_key(b"salt", b"audio", &mut second)
            .expect("key");
        assert_ne!(first, second);
    }
}
