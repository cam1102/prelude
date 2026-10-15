use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use rand_core::{CryptoRng, OsRng, RngCore};
use zeroize::Zeroizing;

use crate::{
    AccessoryIdentity, Error, PairingStore, Result, Tlv8, Tlv8Item, Tlv8Limits, TlvType, crypto,
    srp_server::{SRP_PROOF_LENGTH, SRP_PUBLIC_KEY_LENGTH, SrpServerSession},
};

const PAIRING_AUTHENTICATION_ERROR: u8 = 2;
const PAIR_SETUP_METHOD: u8 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairSetupState {
    AwaitingM1,
    AwaitingM3,
    AwaitingM5,
    Complete,
    Failed,
}

pub struct PairSetup<R = OsRng> {
    store: Arc<PairingStore>,
    identity: Arc<AccessoryIdentity>,
    setup_code: Zeroizing<Vec<u8>>,
    random: R,
    transaction_timeout: Duration,
    started_at: Option<Instant>,
    tlv_limits: Tlv8Limits,
    srp: Option<SrpServerSession>,
    session_key: Option<Zeroizing<[u8; SRP_PROOF_LENGTH]>>,
    state: PairSetupState,
}

impl<R> std::fmt::Debug for PairSetup<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PairSetup")
            .field("setup_code", &"[redacted]")
            .field("transaction_timeout", &self.transaction_timeout)
            .field("started_at", &self.started_at)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl PairSetup<OsRng> {
    pub fn new(
        store: Arc<PairingStore>,
        identity: Arc<AccessoryIdentity>,
        setup_code: impl Into<String>,
        transaction_timeout: Duration,
    ) -> Result<Self> {
        Self::with_rng(store, identity, setup_code, transaction_timeout, OsRng)
    }
}

impl<R> PairSetup<R>
where
    R: RngCore + CryptoRng,
{
    pub fn with_rng(
        store: Arc<PairingStore>,
        identity: Arc<AccessoryIdentity>,
        setup_code: impl Into<String>,
        transaction_timeout: Duration,
        random: R,
    ) -> Result<Self> {
        let setup_code = setup_code.into().into_bytes();
        if setup_code.is_empty()
            || setup_code.len() > 32
            || !setup_code
                .iter()
                .all(|byte| byte.is_ascii_digit() || *byte == b'-')
        {
            return Err(Error::Pairing(
                "setup code must be 1..=32 ASCII digits or hyphens".to_owned(),
            ));
        }
        if transaction_timeout.is_zero()
            || Instant::now().checked_add(transaction_timeout).is_none()
        {
            return Err(Error::Pairing(
                "pair-setup timeout is zero or unrepresentable".to_owned(),
            ));
        }
        Ok(Self {
            store,
            identity,
            setup_code: Zeroizing::new(setup_code),
            random,
            transaction_timeout,
            started_at: None,
            tlv_limits: Tlv8Limits::default(),
            srp: None,
            session_key: None,
            state: PairSetupState::AwaitingM1,
        })
    }

    pub fn state(&self) -> PairSetupState {
        self.state
    }

    pub fn handle(&mut self, body: &[u8]) -> Result<Vec<u8>> {
        self.handle_at(body, Instant::now())
    }

    pub fn handle_at(&mut self, body: &[u8], now: Instant) -> Result<Vec<u8>> {
        if matches!(
            self.state,
            PairSetupState::Complete | PairSetupState::Failed
        ) {
            return Err(Error::NotReady {
                component: "pair-setup",
                state: match self.state {
                    PairSetupState::Complete => "complete",
                    _ => "failed",
                },
            });
        }
        if self.started_at.is_some_and(|started| {
            now.saturating_duration_since(started) > self.transaction_timeout
        }) {
            self.fail();
            return Err(Error::PairingTimeout);
        }

        let request = match Tlv8::decode(body, self.tlv_limits) {
            Ok(request) => request,
            Err(_) => return self.protocol_error(0),
        };
        let request_state = match one_byte(&request, TlvType::STATE) {
            Ok(state) => state,
            Err(_) => return self.protocol_error(0),
        };
        match (self.state, request_state) {
            (PairSetupState::AwaitingM1, 1) => self.m2(&request, now),
            (PairSetupState::AwaitingM3, 3) => self.m4(&request),
            (PairSetupState::AwaitingM5, 5) => self.m6(&request),
            _ => self.protocol_error(request_state),
        }
    }

    fn m2(&mut self, request: &Tlv8, now: Instant) -> Result<Vec<u8>> {
        if request
            .require_only(&[TlvType::STATE, TlvType::METHOD])
            .is_err()
        {
            return self.protocol_error(2);
        }
        match request.optional_unique(TlvType::METHOD) {
            Ok(None) => {}
            Ok(Some(method)) if method == [PAIR_SETUP_METHOD] => {}
            _ => return self.protocol_error(2),
        }
        let srp = SrpServerSession::start(&self.setup_code, &mut self.random)?;
        let response = encode(&[
            Tlv8Item::new(TlvType::STATE, [2]),
            Tlv8Item::new(TlvType::PUBLIC_KEY, srp.public_key().to_vec()),
            Tlv8Item::new(TlvType::SALT, srp.salt().to_vec()),
        ])?;
        self.srp = Some(srp);
        self.started_at = Some(now);
        self.state = PairSetupState::AwaitingM3;
        Ok(response)
    }

    fn m4(&mut self, request: &Tlv8) -> Result<Vec<u8>> {
        if request
            .require_only(&[TlvType::STATE, TlvType::PUBLIC_KEY, TlvType::PROOF])
            .is_err()
        {
            return self.protocol_error(4);
        }
        let client_public = match request.required_unique(TlvType::PUBLIC_KEY) {
            Ok(value) if value.len() == SRP_PUBLIC_KEY_LENGTH => value,
            _ => return self.protocol_error(4),
        };
        let client_proof = match request.required_unique(TlvType::PROOF) {
            Ok(value) if value.len() == SRP_PROOF_LENGTH => value,
            _ => return self.protocol_error(4),
        };
        let Some(srp) = self.srp.take() else {
            return self.protocol_error(4);
        };
        let verification = match srp.verify(client_public, client_proof) {
            Ok(verification) => verification,
            Err(_) => return self.protocol_error(4),
        };
        let response = encode(&[
            Tlv8Item::new(TlvType::STATE, [4]),
            Tlv8Item::new(TlvType::PROOF, verification.server_proof),
        ])?;
        self.session_key = Some(verification.session_key);
        self.state = PairSetupState::AwaitingM5;
        Ok(response)
    }

    fn m6(&mut self, request: &Tlv8) -> Result<Vec<u8>> {
        if request
            .require_only(&[TlvType::STATE, TlvType::ENCRYPTED_DATA])
            .is_err()
        {
            return self.protocol_error(6);
        }
        let encrypted = match request.required_unique(TlvType::ENCRYPTED_DATA) {
            Ok(encrypted) => encrypted,
            Err(_) => return self.protocol_error(6),
        };
        let Some(session_key) = self.session_key.as_ref() else {
            return self.protocol_error(6);
        };
        let mut encryption_key = Zeroizing::new([0; 32]);
        crypto::hkdf_sha512(
            session_key.as_ref(),
            b"Pair-Setup-Encrypt-Salt",
            b"Pair-Setup-Encrypt-Info",
            encryption_key.as_mut(),
        )?;
        let plaintext = match crypto::open(
            &encryption_key,
            &crypto::nonce_label(b"PS-Msg05")?,
            encrypted,
            &[],
        ) {
            Ok(plaintext) => plaintext,
            Err(_) => return self.protocol_error(6),
        };
        let sub = match Tlv8::decode(&plaintext, self.tlv_limits) {
            Ok(sub) => sub,
            Err(_) => return self.protocol_error(6),
        };
        if sub
            .require_only(&[TlvType::IDENTIFIER, TlvType::PUBLIC_KEY, TlvType::SIGNATURE])
            .is_err()
        {
            return self.protocol_error(6);
        }
        let controller_identifier = match sub.required_unique(TlvType::IDENTIFIER) {
            Ok(identifier) => match strict_identifier(identifier) {
                Ok(identifier) => identifier,
                Err(_) => return self.protocol_error(6),
            },
            Err(_) => return self.protocol_error(6),
        };
        let controller_public: [u8; 32] = match sub.required_unique(TlvType::PUBLIC_KEY) {
            Ok(public) => match public.try_into() {
                Ok(public) => public,
                Err(_) => return self.protocol_error(6),
            },
            Err(_) => return self.protocol_error(6),
        };
        let controller_signature: [u8; 64] = match sub.required_unique(TlvType::SIGNATURE) {
            Ok(signature) => match signature.try_into() {
                Ok(signature) => signature,
                Err(_) => return self.protocol_error(6),
            },
            Err(_) => return self.protocol_error(6),
        };
        let mut controller_signing_key = Zeroizing::new([0; 32]);
        crypto::hkdf_sha512(
            session_key.as_ref(),
            b"Pair-Setup-Controller-Sign-Salt",
            b"Pair-Setup-Controller-Sign-Info",
            controller_signing_key.as_mut(),
        )?;
        let mut controller_signed_data = Zeroizing::new(Vec::with_capacity(
            controller_signing_key.len() + controller_identifier.len() + controller_public.len(),
        ));
        controller_signed_data.extend_from_slice(controller_signing_key.as_ref());
        controller_signed_data.extend_from_slice(controller_identifier.as_bytes());
        controller_signed_data.extend_from_slice(&controller_public);
        if !crypto::verify(
            &controller_public,
            &controller_signed_data,
            &controller_signature,
        ) {
            return self.protocol_error(6);
        }

        let accessory_identifier = self.identity.pairing_id().as_bytes();
        let accessory_public = self.identity.public_key();
        let mut accessory_signing_key = Zeroizing::new([0; 32]);
        crypto::hkdf_sha512(
            session_key.as_ref(),
            b"Pair-Setup-Accessory-Sign-Salt",
            b"Pair-Setup-Accessory-Sign-Info",
            accessory_signing_key.as_mut(),
        )?;
        let mut accessory_signed_data = Zeroizing::new(Vec::with_capacity(
            accessory_signing_key.len() + accessory_identifier.len() + accessory_public.len(),
        ));
        accessory_signed_data.extend_from_slice(accessory_signing_key.as_ref());
        accessory_signed_data.extend_from_slice(accessory_identifier);
        accessory_signed_data.extend_from_slice(&accessory_public);
        let accessory_signature = self.identity.sign(&accessory_signed_data);
        let sub_response = encode(&[
            Tlv8Item::new(TlvType::IDENTIFIER, accessory_identifier.to_vec()),
            Tlv8Item::new(TlvType::PUBLIC_KEY, accessory_public),
            Tlv8Item::new(TlvType::SIGNATURE, accessory_signature),
        ])?;
        let sealed = crypto::seal(
            &encryption_key,
            &crypto::nonce_label(b"PS-Msg06")?,
            &sub_response,
            &[],
        )?;
        let response = encode(&[
            Tlv8Item::new(TlvType::STATE, [6]),
            Tlv8Item::new(TlvType::ENCRYPTED_DATA, sealed),
        ])?;
        self.store
            .save_pairing(&controller_identifier, controller_public)?;
        self.session_key = None;
        self.state = PairSetupState::Complete;
        Ok(response)
    }

    fn protocol_error(&mut self, response_state: u8) -> Result<Vec<u8>> {
        self.fail();
        encode(&[
            Tlv8Item::new(TlvType::STATE, [response_state]),
            Tlv8Item::new(TlvType::ERROR, [PAIRING_AUTHENTICATION_ERROR]),
        ])
    }

    fn fail(&mut self) {
        self.srp = None;
        self.session_key = None;
        self.state = PairSetupState::Failed;
    }
}

fn encode(items: &[Tlv8Item]) -> Result<Vec<u8>> {
    Tlv8::new(items.to_vec()).encode(Tlv8Limits::default())
}

fn one_byte(tlv: &Tlv8, kind: u8) -> Result<u8> {
    let value = tlv.required_unique(kind)?;
    if value.len() != 1 {
        return Err(Error::InvalidTlv8(format!(
            "field 0x{kind:02x} is {} bytes; expected 1",
            value.len()
        )));
    }
    Ok(value[0])
}

fn strict_identifier(encoded: &[u8]) -> Result<String> {
    if encoded.is_empty() || encoded.len() > 256 || encoded.contains(&0) {
        return Err(Error::Pairing(
            "controller identifier is empty, too long, or contains a control byte".to_owned(),
        ));
    }
    let identifier = std::str::from_utf8(encoded)
        .map_err(|_| Error::Pairing("controller identifier is not UTF-8".to_owned()))?;
    if identifier.chars().any(char::is_control) {
        return Err(Error::Pairing(
            "controller identifier contains a control character".to_owned(),
        ));
    }
    Ok(identifier.to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ed25519_dalek::SigningKey;
    use rand_core::{CryptoRng, Error as RngError, RngCore};
    use sha2_11::{Digest as _, Sha256};
    use tempfile::TempDir;

    use super::*;

    #[derive(Debug)]
    struct FixedRng(u8);

    impl RngCore for FixedRng {
        fn next_u32(&mut self) -> u32 {
            u32::from(self.0).wrapping_mul(0x0101_0101)
        }

        fn next_u64(&mut self) -> u64 {
            u64::from(self.next_u32()).wrapping_mul(0x0000_0001_0000_0001)
        }

        fn fill_bytes(&mut self, destination: &mut [u8]) {
            destination.fill(self.0);
            self.0 = self.0.wrapping_add(1);
        }

        fn try_fill_bytes(&mut self, destination: &mut [u8]) -> std::result::Result<(), RngError> {
            self.fill_bytes(destination);
            Ok(())
        }
    }

    impl CryptoRng for FixedRng {}

    fn request(state: u8, additional: Vec<Tlv8Item>) -> Vec<u8> {
        let mut items = vec![Tlv8Item::new(TlvType::STATE, [state])];
        items.extend(additional);
        encode(&items).expect("request")
    }

    #[test]
    fn completes_srp_and_the_signed_long_term_key_exchange() {
        let temporary = TempDir::new().expect("temporary");
        let store = Arc::new(PairingStore::open(temporary.path()).expect("store"));
        let identity = Arc::new(
            store
                .load_or_create_identity_with_rng(&mut FixedRng(90))
                .expect("identity"),
        );
        let now = Instant::now();
        let mut setup = PairSetup::with_rng(
            Arc::clone(&store),
            Arc::clone(&identity),
            "3939",
            Duration::from_secs(30),
            FixedRng(1),
        )
        .expect("setup");
        let m2_wire = setup.handle_at(&request(1, Vec::new()), now).expect("M2");
        assert_eq!(
            hex::encode(Sha256::digest(&m2_wire)),
            "ffb7a56634fc2b3bbd039dff35be5f734799d695348afef09992d1757d29e3f5"
        );
        let m2 = Tlv8::decode(&m2_wire, Tlv8Limits::default()).expect("M2 decode");
        assert_eq!(m2.required_unique(TlvType::STATE).expect("state"), [2]);
        let salt = m2.required_unique(TlvType::SALT).expect("salt");
        let server_public = m2.required_unique(TlvType::PUBLIC_KEY).expect("B");

        assert_eq!(salt, [1; 16]);
        assert_eq!(
            hex::encode(Sha256::digest(server_public)),
            "4c12750ef4d8ae4353c3fac630a04fa52f617c4d5ee772304a5fd55e7c957db2"
        );
        let client_public = hex::decode(super::super::srp_server::LIVI_CLIENT_PUBLIC_HEX)
            .expect("client public fixture");
        let client_proof = hex::decode(super::super::srp_server::LIVI_CLIENT_PROOF_HEX)
            .expect("client proof fixture");
        let m4 = Tlv8::decode(
            &setup
                .handle_at(
                    &request(
                        3,
                        vec![
                            Tlv8Item::new(TlvType::PUBLIC_KEY, client_public),
                            Tlv8Item::new(TlvType::PROOF, client_proof),
                        ],
                    ),
                    now,
                )
                .expect("M4"),
            Tlv8Limits::default(),
        )
        .expect("M4 decode");
        assert_eq!(
            hex::encode(m4.required_unique(TlvType::PROOF).expect("M2 proof")),
            super::super::srp_server::LIVI_SERVER_PROOF_HEX
        );
        let session_key = hex::decode(super::super::srp_server::LIVI_SESSION_KEY_HEX)
            .expect("session key fixture");

        let controller = SigningKey::from_bytes(&[11; 32]);
        let controller_id = b"phone-1";
        let controller_public = controller.verifying_key().to_bytes();
        let mut controller_signing_key = [0; 32];
        crypto::hkdf_sha512(
            &session_key,
            b"Pair-Setup-Controller-Sign-Salt",
            b"Pair-Setup-Controller-Sign-Info",
            &mut controller_signing_key,
        )
        .expect("controller signing key");
        let mut signed = Vec::new();
        signed.extend_from_slice(&controller_signing_key);
        signed.extend_from_slice(controller_id);
        signed.extend_from_slice(&controller_public);
        let signature = crypto::sign(&controller, &signed);
        let sub = encode(&[
            Tlv8Item::new(TlvType::IDENTIFIER, controller_id),
            Tlv8Item::new(TlvType::PUBLIC_KEY, controller_public),
            Tlv8Item::new(TlvType::SIGNATURE, signature),
        ])
        .expect("M5 sub-TLV");
        let mut encryption_key = [0; 32];
        crypto::hkdf_sha512(
            &session_key,
            b"Pair-Setup-Encrypt-Salt",
            b"Pair-Setup-Encrypt-Info",
            &mut encryption_key,
        )
        .expect("encryption key");
        let encrypted = crypto::seal(
            &encryption_key,
            &crypto::nonce_label(b"PS-Msg05").expect("nonce"),
            &sub,
            &[],
        )
        .expect("sealed M5");
        let m6 = setup
            .handle_at(
                &request(5, vec![Tlv8Item::new(TlvType::ENCRYPTED_DATA, encrypted)]),
                now,
            )
            .expect("M6");
        assert_eq!(setup.state(), PairSetupState::Complete);
        assert_eq!(
            store.pairing("phone-1").expect("stored"),
            Some(controller_public)
        );
        let m6 = Tlv8::decode(&m6, Tlv8Limits::default()).expect("M6 decode");
        let sub = crypto::open(
            &encryption_key,
            &crypto::nonce_label(b"PS-Msg06").expect("nonce"),
            m6.required_unique(TlvType::ENCRYPTED_DATA)
                .expect("encrypted data"),
            &[],
        )
        .expect("M6 open");
        let sub = Tlv8::decode(&sub, Tlv8Limits::default()).expect("M6 sub-TLV");
        assert_eq!(
            sub.required_unique(TlvType::PUBLIC_KEY)
                .expect("accessory key"),
            identity.public_key()
        );
    }

    #[test]
    fn malformed_or_timed_out_transactions_are_terminal() {
        let temporary = TempDir::new().expect("temporary");
        let store = Arc::new(PairingStore::open(temporary.path()).expect("store"));
        let identity = Arc::new(store.load_or_create_identity().expect("identity"));
        let now = Instant::now();
        let mut malformed = PairSetup::with_rng(
            Arc::clone(&store),
            Arc::clone(&identity),
            "3939",
            Duration::from_secs(1),
            FixedRng(1),
        )
        .expect("setup");
        let error_reply = malformed.handle_at(&[6, 1, 3], now).expect("error reply");
        assert_eq!(malformed.state(), PairSetupState::Failed);
        assert_eq!(
            Tlv8::decode(&error_reply, Tlv8Limits::default())
                .expect("decode")
                .required_unique(TlvType::ERROR)
                .expect("error"),
            [PAIRING_AUTHENTICATION_ERROR]
        );

        let mut timed =
            PairSetup::with_rng(store, identity, "3939", Duration::from_secs(1), FixedRng(1))
                .expect("setup");
        timed.handle_at(&request(1, Vec::new()), now).expect("M2");
        assert_eq!(
            timed
                .handle_at(&request(3, Vec::new()), now + Duration::from_secs(2))
                .expect_err("timeout"),
            Error::PairingTimeout
        );
        assert_eq!(timed.state(), PairSetupState::Failed);
    }
}
