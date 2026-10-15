use aes::Aes128;
use ctr::cipher::{KeyIvInit, StreamCipher};
use rand_core::{CryptoRng, OsRng, RngCore};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use crate::{Error, PairVerification, Result};

pub const MFISAP_VERSION: u8 = 1;
const PUBLIC_KEY_LEN: usize = 32;
const REQUEST_LEN: usize = 1 + PUBLIC_KEY_LEN;
const REQUIRED_PROTOCOL_MAJOR: u8 = 3;
const CERTIFICATE_MIN_LEN: usize = 607;
const CERTIFICATE_MAX_LEN: usize = 609;
const SIGNATURE_LEN: usize = 64;

type Aes128Ctr = ctr::Ctr128BE<Aes128>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MfiSapRequest {
    peer_public_key: [u8; PUBLIC_KEY_LEN],
}

impl MfiSapRequest {
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() != REQUEST_LEN {
            return Err(Error::InvalidMfiSap(format!(
                "request is {} bytes; expected {REQUEST_LEN}",
                body.len()
            )));
        }
        if body[0] != MFISAP_VERSION {
            return Err(Error::InvalidMfiSap(format!(
                "unsupported version {}",
                body[0]
            )));
        }
        let mut peer_public_key = [0; PUBLIC_KEY_LEN];
        peer_public_key.copy_from_slice(&body[1..]);
        Ok(Self { peer_public_key })
    }

    pub fn peer_public_key(&self) -> &[u8; PUBLIC_KEY_LEN] {
        &self.peer_public_key
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct SignerError {
    message: String,
}

impl SignerError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub trait MfiSigner: std::fmt::Debug + Send {
    fn protocol_major(&self) -> u8;
    fn certificate(&mut self) -> std::result::Result<Vec<u8>, SignerError>;
    fn sign_digest(&mut self, digest: &[u8; 32]) -> std::result::Result<Vec<u8>, SignerError>;
}

impl<T> MfiSigner for prelude_mfi::MfiAuth<T>
where
    T: prelude_mfi::Transport + Send,
{
    fn protocol_major(&self) -> u8 {
        self.device_info().protocol_major
    }

    fn certificate(&mut self) -> std::result::Result<Vec<u8>, SignerError> {
        prelude_mfi::MfiAuth::certificate(self).map_err(|error| SignerError::new(error.to_string()))
    }

    fn sign_digest(&mut self, digest: &[u8; 32]) -> std::result::Result<Vec<u8>, SignerError> {
        prelude_mfi::MfiAuth::sign(self, digest)
            .map(|signature| signature.to_vec())
            .map_err(|error| SignerError::new(error.to_string()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MfiSapState {
    AwaitingRequest,
    ProofIssued,
    Failed,
}

impl MfiSapState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingRequest => "awaiting_request",
            Self::ProofIssued => "proof_issued",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MfiSapProof {
    response: Vec<u8>,
    accessory_public_key: [u8; PUBLIC_KEY_LEN],
}

impl MfiSapProof {
    pub fn response(&self) -> &[u8] {
        &self.response
    }

    pub fn accessory_public_key(&self) -> &[u8; PUBLIC_KEY_LEN] {
        &self.accessory_public_key
    }
}

#[derive(Debug)]
pub struct MfiSapResponder<S, R = OsRng> {
    signer: S,
    random: R,
    state: MfiSapState,
}

impl<S: MfiSigner> MfiSapResponder<S, OsRng> {
    pub fn new(signer: S) -> Self {
        Self::with_rng(signer, OsRng)
    }
}

impl<S: MfiSigner, R: RngCore + CryptoRng> MfiSapResponder<S, R> {
    pub fn with_rng(signer: S, random: R) -> Self {
        Self {
            signer,
            random,
            state: MfiSapState::AwaitingRequest,
        }
    }

    pub fn state(&self) -> MfiSapState {
        self.state
    }

    pub fn respond(
        &mut self,
        request: MfiSapRequest,
        _pairing: &PairVerification,
    ) -> Result<MfiSapProof> {
        if self.state != MfiSapState::AwaitingRequest {
            return Err(Error::NotReady {
                component: "MFiSAP responder",
                state: self.state.as_str(),
            });
        }
        if self.signer.protocol_major() != REQUIRED_PROTOCOL_MAJOR {
            self.state = MfiSapState::Failed;
            return Err(Error::Unsupported(
                "MFiSAP responder requires an Authentication 3.x coprocessor",
            ));
        }

        let secret = StaticSecret::random_from_rng(&mut self.random);
        let public = PublicKey::from(&secret);
        let peer = PublicKey::from(*request.peer_public_key());
        let shared = secret.diffie_hellman(&peer);
        if bool::from(shared.as_bytes().ct_eq(&[0; 32])) {
            self.state = MfiSapState::Failed;
            return Err(Error::Crypto("MFiSAP X25519 shared secret is all zero"));
        }

        let certificate = self
            .signer
            .certificate()
            .map_err(|error| self.signer_failed(error))?;
        if !(CERTIFICATE_MIN_LEN..=CERTIFICATE_MAX_LEN).contains(&certificate.len()) {
            self.state = MfiSapState::Failed;
            return Err(Error::Signer(format!(
                "MFi certificate is {} bytes; expected {CERTIFICATE_MIN_LEN}..={CERTIFICATE_MAX_LEN}",
                certificate.len()
            )));
        }
        let digest: [u8; 32] = Sha256::new()
            .chain_update(public.as_bytes())
            .chain_update(request.peer_public_key())
            .finalize()
            .into();
        let signature = self
            .signer
            .sign_digest(&digest)
            .map_err(|error| self.signer_failed(error))?;
        if signature.len() != SIGNATURE_LEN {
            self.state = MfiSapState::Failed;
            return Err(Error::Signer(format!(
                "MFi signature is {} bytes; expected {SIGNATURE_LEN}",
                signature.len()
            )));
        }

        let key_digest: [u8; 20] = Sha1::new()
            .chain_update(b"AES-KEY")
            .chain_update(shared.as_bytes())
            .finalize()
            .into();
        let key_digest = Zeroizing::new(key_digest);
        let iv_digest: [u8; 20] = Sha1::new()
            .chain_update(b"AES-IV")
            .chain_update(shared.as_bytes())
            .finalize()
            .into();
        let iv_digest = Zeroizing::new(iv_digest);
        let mut encrypted_signature = signature;
        Aes128Ctr::new((&key_digest[..16]).into(), (&iv_digest[..16]).into())
            .apply_keystream(&mut encrypted_signature);

        let certificate_len = u32::try_from(certificate.len())
            .map_err(|_| Error::Signer("MFi certificate is too large".to_owned()))?;
        let signature_len = u32::try_from(encrypted_signature.len())
            .map_err(|_| Error::Signer("MFi signature is too large".to_owned()))?;
        let mut response = Vec::with_capacity(
            PUBLIC_KEY_LEN + 4 + certificate.len() + 4 + encrypted_signature.len(),
        );
        response.extend_from_slice(public.as_bytes());
        response.extend_from_slice(&certificate_len.to_be_bytes());
        response.extend_from_slice(&certificate);
        response.extend_from_slice(&signature_len.to_be_bytes());
        response.extend_from_slice(&encrypted_signature);

        self.state = MfiSapState::ProofIssued;
        Ok(MfiSapProof {
            response,
            accessory_public_key: *public.as_bytes(),
        })
    }

    fn signer_failed(&mut self, error: SignerError) -> Error {
        self.state = MfiSapState::Failed;
        Error::Signer(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use rand_core::{CryptoRng, Error as RngError, RngCore};

    use super::*;

    #[derive(Debug)]
    struct FixedSigner {
        major: u8,
        certificate: Vec<u8>,
        signature: Vec<u8>,
    }

    impl MfiSigner for FixedSigner {
        fn protocol_major(&self) -> u8 {
            self.major
        }

        fn certificate(&mut self) -> std::result::Result<Vec<u8>, SignerError> {
            Ok(self.certificate.clone())
        }

        fn sign_digest(&mut self, digest: &[u8; 32]) -> std::result::Result<Vec<u8>, SignerError> {
            assert_ne!(digest, &[0; 32]);
            Ok(self.signature.clone())
        }
    }

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

    fn request() -> MfiSapRequest {
        let peer_secret = StaticSecret::from([11; 32]);
        let peer_public = PublicKey::from(&peer_secret);
        let mut bytes = vec![MFISAP_VERSION];
        bytes.extend_from_slice(peer_public.as_bytes());
        MfiSapRequest::parse(&bytes).expect("request")
    }

    fn pairing() -> PairVerification {
        PairVerification::new("phone", [3; 32]).expect("pairing")
    }

    #[test]
    fn proof_has_bounded_binary_layout_and_is_one_shot() {
        let signer = FixedSigner {
            major: 3,
            certificate: vec![0x30; 608],
            signature: vec![0x44; 64],
        };
        let mut responder = MfiSapResponder::with_rng(signer, FixedRng(7));
        let proof = responder.respond(request(), &pairing()).expect("proof");
        assert_eq!(
            hex::encode(Sha256::digest(proof.response())),
            "c03abc66c0da3437f5147ce8820762166d065c8c449210af80ef6eaddbe5b077"
        );
        assert_eq!(proof.response().len(), 32 + 4 + 608 + 4 + 64);
        assert_eq!(
            u32::from_be_bytes(
                proof.response()[32..36]
                    .try_into()
                    .expect("certificate length")
            ),
            608
        );
        assert_eq!(responder.state(), MfiSapState::ProofIssued);

        let peer_secret = StaticSecret::from([11; 32]);
        let accessory_public = PublicKey::from(*proof.accessory_public_key());
        let shared = peer_secret.diffie_hellman(&accessory_public);
        let key_digest = Sha1::new()
            .chain_update(b"AES-KEY")
            .chain_update(shared.as_bytes())
            .finalize();
        let iv_digest = Sha1::new()
            .chain_update(b"AES-IV")
            .chain_update(shared.as_bytes())
            .finalize();
        let signature_offset = 32 + 4 + 608 + 4;
        let mut decrypted_signature = proof.response()[signature_offset..].to_vec();
        Aes128Ctr::new((&key_digest[..16]).into(), (&iv_digest[..16]).into())
            .apply_keystream(&mut decrypted_signature);
        assert_eq!(decrypted_signature, vec![0x44; 64]);

        assert!(matches!(
            responder.respond(request(), &pairing()),
            Err(Error::NotReady { .. })
        ));
    }

    #[test]
    fn malformed_request_is_rejected() {
        assert!(matches!(
            MfiSapRequest::parse(&[1; 32]),
            Err(Error::InvalidMfiSap(_))
        ));
        let mut request = [0; REQUEST_LEN];
        request[0] = 2;
        assert!(matches!(
            MfiSapRequest::parse(&request),
            Err(Error::InvalidMfiSap(_))
        ));
    }

    #[test]
    fn old_authentication_protocol_is_unsupported_and_terminal() {
        let signer = FixedSigner {
            major: 2,
            certificate: vec![1],
            signature: vec![2],
        };
        let mut responder = MfiSapResponder::with_rng(signer, FixedRng(5));
        assert!(matches!(
            responder.respond(request(), &pairing()),
            Err(Error::Unsupported(_))
        ));
        assert_eq!(responder.state(), MfiSapState::Failed);
    }
}
