use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use rand_core::{CryptoRng, OsRng, RngCore};
use subtle::ConstantTimeEq;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use crate::{
    AccessoryIdentity, Error, PairVerification, PairingStore, Result, Tlv8, Tlv8Item, Tlv8Limits,
    TlvType, crypto,
};

const PAIRING_AUTHENTICATION_ERROR: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairVerifyState {
    AwaitingM1,
    AwaitingM3,
    Verified,
    Failed,
}

pub struct PairVerify<R = OsRng> {
    store: Arc<PairingStore>,
    identity: Arc<AccessoryIdentity>,
    random: R,
    transaction_timeout: Duration,
    started_at: Option<Instant>,
    client_public: Option<[u8; 32]>,
    accessory_public: Option<[u8; 32]>,
    shared_secret: Option<Zeroizing<[u8; 32]>>,
    encryption_key: Option<Zeroizing<[u8; 32]>>,
    verification: Option<PairVerification>,
    state: PairVerifyState,
    failure_reason: Option<&'static str>,
}

impl<R> std::fmt::Debug for PairVerify<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PairVerify")
            .field("transaction_timeout", &self.transaction_timeout)
            .field("started_at", &self.started_at)
            .field(
                "shared_secret",
                &self.shared_secret.as_ref().map(|_| "[redacted]"),
            )
            .field(
                "encryption_key",
                &self.encryption_key.as_ref().map(|_| "[redacted]"),
            )
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl PairVerify<OsRng> {
    pub(crate) fn restart(&mut self) -> Result<()> {
        *self = Self::new(
            Arc::clone(&self.store),
            Arc::clone(&self.identity),
            self.transaction_timeout,
        )?;
        Ok(())
    }

    pub fn new(
        store: Arc<PairingStore>,
        identity: Arc<AccessoryIdentity>,
        transaction_timeout: Duration,
    ) -> Result<Self> {
        Self::with_rng(store, identity, transaction_timeout, OsRng)
    }
}

impl<R> PairVerify<R>
where
    R: RngCore + CryptoRng,
{
    pub fn with_rng(
        store: Arc<PairingStore>,
        identity: Arc<AccessoryIdentity>,
        transaction_timeout: Duration,
        random: R,
    ) -> Result<Self> {
        if transaction_timeout.is_zero()
            || Instant::now().checked_add(transaction_timeout).is_none()
        {
            return Err(Error::Pairing(
                "pair-verify timeout is zero or unrepresentable".to_owned(),
            ));
        }
        Ok(Self {
            store,
            identity,
            random,
            transaction_timeout,
            started_at: None,
            client_public: None,
            accessory_public: None,
            shared_secret: None,
            encryption_key: None,
            verification: None,
            state: PairVerifyState::AwaitingM1,
            failure_reason: None,
        })
    }

    pub fn state(&self) -> PairVerifyState {
        self.state
    }

    pub fn failure_reason(&self) -> Option<&'static str> {
        self.failure_reason
    }

    pub fn verification(&self) -> Option<&PairVerification> {
        self.verification.as_ref()
    }

    pub fn take_verification(&mut self) -> Option<PairVerification> {
        self.verification.take()
    }

    pub fn handle(&mut self, body: &[u8]) -> Result<Vec<u8>> {
        self.handle_at(body, Instant::now())
    }

    pub fn handle_at(&mut self, body: &[u8], now: Instant) -> Result<Vec<u8>> {
        if matches!(
            self.state,
            PairVerifyState::Verified | PairVerifyState::Failed
        ) {
            return Err(Error::NotReady {
                component: "pair-verify",
                state: if self.state == PairVerifyState::Verified {
                    "verified"
                } else {
                    "failed"
                },
            });
        }
        if self.started_at.is_some_and(|started| {
            now.saturating_duration_since(started) > self.transaction_timeout
        }) {
            self.fail();
            return Err(Error::PairingTimeout);
        }
        let request = match Tlv8::decode(body, Tlv8Limits::default()) {
            Ok(request) => request,
            Err(_) => return self.protocol_error(0),
        };
        let request_state = match one_byte(&request, TlvType::STATE) {
            Ok(state) => state,
            Err(_) => return self.protocol_error(0),
        };
        match (self.state, request_state) {
            (PairVerifyState::AwaitingM1, 1) => self.m2(&request, now),
            (PairVerifyState::AwaitingM3, 3) => self.m4(&request),
            _ => self.protocol_error(request_state),
        }
    }

    fn m2(&mut self, request: &Tlv8, now: Instant) -> Result<Vec<u8>> {
        if request
            .require_only(&[TlvType::STATE, TlvType::PUBLIC_KEY])
            .is_err()
        {
            return self.protocol_error(2);
        }
        let client_public: [u8; 32] = match request.required_unique(TlvType::PUBLIC_KEY) {
            Ok(public) => match public.try_into() {
                Ok(public) => public,
                Err(_) => return self.protocol_error(2),
            },
            Err(_) => return self.protocol_error(2),
        };
        let secret = StaticSecret::random_from_rng(&mut self.random);
        let accessory_public = PublicKey::from(&secret).to_bytes();
        let shared = secret
            .diffie_hellman(&PublicKey::from(client_public))
            .to_bytes();
        if bool::from(shared.ct_eq(&[0; 32])) {
            return self.protocol_error(2);
        }
        let shared = Zeroizing::new(shared);
        let mut encryption_key = Zeroizing::new([0; 32]);
        crypto::hkdf_sha512(
            shared.as_ref(),
            b"Pair-Verify-Encrypt-Salt",
            b"Pair-Verify-Encrypt-Info",
            encryption_key.as_mut(),
        )?;
        let accessory_identifier = self.identity.pairing_id().as_bytes();
        let mut signed = Vec::with_capacity(
            accessory_public.len() + accessory_identifier.len() + client_public.len(),
        );
        signed.extend_from_slice(&accessory_public);
        signed.extend_from_slice(accessory_identifier);
        signed.extend_from_slice(&client_public);
        let signature = self.identity.sign(&signed);
        let sub = encode(&[
            Tlv8Item::new(TlvType::IDENTIFIER, accessory_identifier),
            Tlv8Item::new(TlvType::SIGNATURE, signature),
        ])?;
        let sealed = crypto::seal(
            &encryption_key,
            &crypto::nonce_label(b"PV-Msg02")?,
            &sub,
            &[],
        )?;
        let response = encode(&[
            Tlv8Item::new(TlvType::STATE, [2]),
            Tlv8Item::new(TlvType::PUBLIC_KEY, accessory_public),
            Tlv8Item::new(TlvType::ENCRYPTED_DATA, sealed),
        ])?;
        self.client_public = Some(client_public);
        self.accessory_public = Some(accessory_public);
        self.shared_secret = Some(shared);
        self.encryption_key = Some(encryption_key);
        self.started_at = Some(now);
        self.state = PairVerifyState::AwaitingM3;
        Ok(response)
    }

    fn m4(&mut self, request: &Tlv8) -> Result<Vec<u8>> {
        if request
            .require_only(&[TlvType::STATE, TlvType::ENCRYPTED_DATA])
            .is_err()
        {
            return self.protocol_error(4);
        }
        let encrypted = match request.required_unique(TlvType::ENCRYPTED_DATA) {
            Ok(encrypted) => encrypted,
            Err(_) => return self.protocol_error(4),
        };
        let (Some(client_public), Some(accessory_public)) =
            (self.client_public, self.accessory_public)
        else {
            return self.protocol_error(4);
        };
        let Some(encryption_key) = self.encryption_key.take() else {
            return self.protocol_error(4);
        };
        let plaintext = match crypto::open(
            &encryption_key,
            &crypto::nonce_label(b"PV-Msg03")?,
            encrypted,
            &[],
        ) {
            Ok(plaintext) => plaintext,
            Err(_) => return self.protocol_error(4),
        };
        let sub = match Tlv8::decode(&plaintext, Tlv8Limits::default()) {
            Ok(sub) => sub,
            Err(_) => return self.protocol_error(4),
        };
        if sub
            .require_only(&[TlvType::IDENTIFIER, TlvType::SIGNATURE])
            .is_err()
        {
            return self.protocol_error(4);
        }
        let controller_identifier = match sub.required_unique(TlvType::IDENTIFIER) {
            Ok(identifier) => match strict_identifier(identifier) {
                Ok(identifier) => identifier,
                Err(_) => return self.protocol_error(4),
            },
            Err(_) => return self.protocol_error(4),
        };
        let controller_signature: [u8; 64] = match sub.required_unique(TlvType::SIGNATURE) {
            Ok(signature) => match signature.try_into() {
                Ok(signature) => signature,
                Err(_) => return self.protocol_error(4),
            },
            Err(_) => return self.protocol_error(4),
        };
        let Some(controller_public) = self.store.pairing(&controller_identifier)? else {
            self.failure_reason = Some("pair-verify controller pairing is not stored");
            return self.protocol_error(4);
        };
        let mut signed = Vec::with_capacity(
            client_public.len() + controller_identifier.len() + accessory_public.len(),
        );
        signed.extend_from_slice(&client_public);
        signed.extend_from_slice(controller_identifier.as_bytes());
        signed.extend_from_slice(&accessory_public);
        if !crypto::verify(&controller_public, &signed, &controller_signature) {
            self.failure_reason = Some("pair-verify controller signature is invalid");
            return self.protocol_error(4);
        }
        let Some(shared_secret) = self.shared_secret.take() else {
            return self.protocol_error(4);
        };
        self.verification = Some(PairVerification::new(
            controller_identifier,
            *shared_secret,
        )?);
        self.state = PairVerifyState::Verified;
        encode(&[Tlv8Item::new(TlvType::STATE, [4])])
    }

    fn protocol_error(&mut self, response_state: u8) -> Result<Vec<u8>> {
        self.fail();
        encode(&[
            Tlv8Item::new(TlvType::STATE, [response_state]),
            Tlv8Item::new(TlvType::ERROR, [PAIRING_AUTHENTICATION_ERROR]),
        ])
    }

    fn fail(&mut self) {
        self.client_public = None;
        self.accessory_public = None;
        self.shared_secret = None;
        self.encryption_key = None;
        self.verification = None;
        self.state = PairVerifyState::Failed;
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
    use ed25519_dalek::SigningKey;
    use rand_core::{CryptoRng, Error as RngError, RngCore};
    use sha2::{Digest as _, Sha256};
    use tempfile::TempDir;

    use super::*;
    use crate::{ControlCipher, ControlKeys};

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
    fn verifies_a_persisted_controller_and_derives_directional_keys() {
        let temporary = TempDir::new().expect("temporary");
        let store = Arc::new(PairingStore::open(temporary.path()).expect("store"));
        let identity = Arc::new(
            store
                .load_or_create_identity_with_rng(&mut FixedRng(90))
                .expect("identity"),
        );
        let controller = SigningKey::from_bytes(&[11; 32]);
        store
            .save_pairing("phone-1", controller.verifying_key().to_bytes())
            .expect("pairing");
        let mut verify = PairVerify::with_rng(
            store,
            Arc::clone(&identity),
            Duration::from_secs(30),
            FixedRng(7),
        )
        .expect("verify");
        let client_secret = StaticSecret::from([13; 32]);
        let client_public = PublicKey::from(&client_secret).to_bytes();
        let m2_wire = verify
            .handle(&request(
                1,
                vec![Tlv8Item::new(TlvType::PUBLIC_KEY, client_public)],
            ))
            .expect("M2");
        assert_eq!(
            hex::encode(Sha256::digest(&m2_wire)),
            "f72cd02abad72e65bd7e079c2befaa5fd0356a43b2bd1b5f548d86e2985daa8b"
        );
        let m2 = Tlv8::decode(&m2_wire, Tlv8Limits::default()).expect("M2 decode");
        let accessory_public: [u8; 32] = m2
            .required_unique(TlvType::PUBLIC_KEY)
            .expect("accessory public")
            .try_into()
            .expect("length");
        let shared = client_secret
            .diffie_hellman(&PublicKey::from(accessory_public))
            .to_bytes();
        let mut encryption_key = [0; 32];
        crypto::hkdf_sha512(
            &shared,
            b"Pair-Verify-Encrypt-Salt",
            b"Pair-Verify-Encrypt-Info",
            &mut encryption_key,
        )
        .expect("encryption key");
        let m2_sub = crypto::open(
            &encryption_key,
            &crypto::nonce_label(b"PV-Msg02").expect("nonce"),
            m2.required_unique(TlvType::ENCRYPTED_DATA)
                .expect("encrypted"),
            &[],
        )
        .expect("M2 open");
        let m2_sub = Tlv8::decode(&m2_sub, Tlv8Limits::default()).expect("sub-TLV");
        let accessory_identifier = m2_sub
            .required_unique(TlvType::IDENTIFIER)
            .expect("identifier");
        let mut accessory_signed = Vec::new();
        accessory_signed.extend_from_slice(&accessory_public);
        accessory_signed.extend_from_slice(accessory_identifier);
        accessory_signed.extend_from_slice(&client_public);
        let accessory_signature: [u8; 64] = m2_sub
            .required_unique(TlvType::SIGNATURE)
            .expect("signature")
            .try_into()
            .expect("length");
        assert!(crypto::verify(
            &identity.public_key(),
            &accessory_signed,
            &accessory_signature
        ));

        let mut controller_signed = Vec::new();
        controller_signed.extend_from_slice(&client_public);
        controller_signed.extend_from_slice(b"phone-1");
        controller_signed.extend_from_slice(&accessory_public);
        let controller_signature = crypto::sign(&controller, &controller_signed);
        let sub = encode(&[
            Tlv8Item::new(TlvType::IDENTIFIER, b"phone-1"),
            Tlv8Item::new(TlvType::SIGNATURE, controller_signature),
        ])
        .expect("M3 sub-TLV");
        let sealed = crypto::seal(
            &encryption_key,
            &crypto::nonce_label(b"PV-Msg03").expect("nonce"),
            &sub,
            &[],
        )
        .expect("M3 seal");
        let m4 = verify
            .handle(&request(
                3,
                vec![Tlv8Item::new(TlvType::ENCRYPTED_DATA, sealed)],
            ))
            .expect("M4");
        assert_eq!(m4, [TlvType::STATE, 1, 4]);
        assert_eq!(verify.state(), PairVerifyState::Verified);
        assert_eq!(
            verify.verification().expect("verification").controller_id(),
            "phone-1"
        );

        let mut controller_read = [0; 32];
        let mut controller_write = [0; 32];
        crypto::hkdf_sha512(
            &shared,
            b"Control-Salt",
            b"Control-Read-Encryption-Key",
            &mut controller_read,
        )
        .expect("controller read");
        crypto::hkdf_sha512(
            &shared,
            b"Control-Salt",
            b"Control-Write-Encryption-Key",
            &mut controller_write,
        )
        .expect("controller write");
        let mut phone = ControlCipher::new(
            ControlKeys::new(controller_read, controller_write).expect("phone keys"),
        )
        .expect("phone");
        let mut accessory =
            ControlCipher::from_verification(verify.verification().expect("verification"))
                .expect("accessory");
        assert_eq!(
            accessory
                .decrypt(&phone.encrypt(b"ping").expect("phone encrypt"))
                .expect("accessory decrypt"),
            b"ping"
        );
    }

    #[test]
    fn unknown_controller_timeout_and_replay_fail_closed() {
        let temporary = TempDir::new().expect("temporary");
        let store = Arc::new(PairingStore::open(temporary.path()).expect("store"));
        let identity = Arc::new(store.load_or_create_identity().expect("identity"));
        let now = Instant::now();
        let mut verify =
            PairVerify::with_rng(store, identity, Duration::from_millis(1), FixedRng(7))
                .expect("verify");
        let client_secret = StaticSecret::from([13; 32]);
        let m1 = request(
            1,
            vec![Tlv8Item::new(
                TlvType::PUBLIC_KEY,
                PublicKey::from(&client_secret).to_bytes(),
            )],
        );
        verify.handle_at(&m1, now).expect("M2");
        assert_eq!(
            verify
                .handle_at(&request(3, Vec::new()), now + Duration::from_millis(2))
                .expect_err("timeout"),
            Error::PairingTimeout
        );
        assert_eq!(verify.state(), PairVerifyState::Failed);
        assert!(matches!(
            verify.handle_at(&m1, now),
            Err(Error::NotReady { .. })
        ));
    }
}
