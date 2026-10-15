use rand_core::{CryptoRng, RngCore};
use sha2_11::Sha512;
use srp::{ClientG3072, ServerG3072};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::{Error, Result};

pub(crate) const SRP_PUBLIC_KEY_LENGTH: usize = 384;
pub(crate) const SRP_PROOF_LENGTH: usize = 64;
pub(crate) const SRP_SALT_LENGTH: usize = 16;
const SRP_EPHEMERAL_SECRET_LENGTH: usize = 32;
const USERNAME: &[u8] = b"Pair-Setup";

#[cfg(test)]
pub(crate) const LIVI_CLIENT_PUBLIC_HEX: &str = concat!(
    "5fda2ae90cc206cf5dc925c3c6c18a6209dd897c8bba709b22bb0ff0306180f9",
    "fef271c16ac724e72e5d2350b37edd8f2207d09e37b5772034530757d41be142",
    "78cc76b6dd10091c2fbe48181a5a99d24f4597982e57848e51c7938063ceee1ba",
    "a9597c202fb490e867e791d141f91a4b4bfd6f6ca07f4714769c49b18a0e56e0",
    "6158ac148e82fba2d18934df0243b0753f4e393cd2a4b0098f81fbc3b6a3d3b4",
    "e200f7a99305d45c8ad48e4e457ddade0db7ccccb0ab027aa5e2247c1b65713b",
    "1676712a9b6f13ec7405251bdd03f7ea05e7829890b3d8511f484955755683b650",
    "f7828d095c08b981e68fad1213c109742c5a5ea3f224528e91f61b85f6ab5f67",
    "175654c4ff70f77d70c915df4d247f3b1bf47df66195a64040f35a6f6949fcf1",
    "f8c5d33a28f07def042b00e2d7745039ef3c9ad706d42082153e12d7ecff3c4f",
    "cf6be33eec74c2a5c8e278897e8dd72183a577bc849c4f1cae643e0453bc581b",
    "e1ee4732ae87bf805c8fa60886d609fb7da64d150f2cda3a926ebf43b1faa",
);
#[cfg(test)]
pub(crate) const LIVI_CLIENT_PROOF_HEX: &str = "7a08115ddc63c2671ae7c76a7369b073e65b4268db379541b94366ba30a6bd99bdc06336ffcf861846dbb6481cc58cdc081f647f1e533641f4da25131f169fa1";
#[cfg(test)]
pub(crate) const LIVI_SERVER_PROOF_HEX: &str = "3abf081f69ca9df97e23ca2f36cb16e8cd15f24d34613614ff7c829a0a91d123db0a0e29def92b85e84cdcc0232d9f6f7392c8c10f822a251af11f0791b9203d";
#[cfg(test)]
pub(crate) const LIVI_SESSION_KEY_HEX: &str = "96cc3161be6a7354232cffb65b3af390def3aced80795b50feecc7f5ce7ff572a427ec23f1e5832ff30a3adb9a79d30bf579698a09309a7477e82ef3f0cdc659";

pub(crate) struct SrpServerSession {
    salt: [u8; SRP_SALT_LENGTH],
    public_key: [u8; SRP_PUBLIC_KEY_LENGTH],
    private_ephemeral: Zeroizing<[u8; SRP_EPHEMERAL_SECRET_LENGTH]>,
    password_verifier: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for SrpServerSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SrpServerSession")
            .field("salt", &hex::encode(self.salt))
            .field("public_key", &hex::encode(self.public_key))
            .field("private_ephemeral", &"[redacted]")
            .field("password_verifier", &"[redacted]")
            .finish()
    }
}

pub(crate) struct SrpVerification {
    pub session_key: Zeroizing<[u8; SRP_PROOF_LENGTH]>,
    pub server_proof: [u8; SRP_PROOF_LENGTH],
}

impl SrpServerSession {
    pub fn start<R>(setup_code: &[u8], random: &mut R) -> Result<Self>
    where
        R: RngCore + CryptoRng,
    {
        if setup_code.is_empty() {
            return Err(Error::Pairing("setup code is empty".to_owned()));
        }
        let mut salt = [0; SRP_SALT_LENGTH];
        let mut private_ephemeral = Zeroizing::new([0; SRP_EPHEMERAL_SECRET_LENGTH]);
        random.fill_bytes(&mut salt);
        random.fill_bytes(private_ephemeral.as_mut());
        if bool::from(private_ephemeral.ct_eq(&[0; SRP_EPHEMERAL_SECRET_LENGTH])) {
            return Err(Error::Crypto("SRP ephemeral secret is all zero"));
        }

        let client = ClientG3072::<Sha512>::new();
        let password_verifier =
            Zeroizing::new(client.compute_verifier(USERNAME, setup_code, &salt));
        let server = ServerG3072::<Sha512>::new_with_options(true);
        let public_key = left_pad::<SRP_PUBLIC_KEY_LENGTH>(
            &server
                .compute_public_ephemeral(private_ephemeral.as_ref(), password_verifier.as_ref()),
        )?;
        Ok(Self {
            salt,
            public_key,
            private_ephemeral,
            password_verifier,
        })
    }

    pub fn salt(&self) -> &[u8; SRP_SALT_LENGTH] {
        &self.salt
    }

    pub fn public_key(&self) -> &[u8; SRP_PUBLIC_KEY_LENGTH] {
        &self.public_key
    }

    pub fn verify(self, client_public_key: &[u8], client_proof: &[u8]) -> Result<SrpVerification> {
        if client_public_key.len() != SRP_PUBLIC_KEY_LENGTH {
            return Err(Error::Pairing(format!(
                "SRP client public key is {} bytes; expected {SRP_PUBLIC_KEY_LENGTH}",
                client_public_key.len()
            )));
        }
        if client_proof.len() != SRP_PROOF_LENGTH {
            return Err(Error::Pairing(format!(
                "SRP client proof is {} bytes; expected {SRP_PROOF_LENGTH}",
                client_proof.len()
            )));
        }
        let server = ServerG3072::<Sha512>::new_with_options(true);
        let verifier = server
            .process_reply(
                USERNAME,
                &self.salt,
                self.private_ephemeral.as_ref(),
                self.password_verifier.as_ref(),
                client_public_key,
            )
            .map_err(|_| Error::Pairing("SRP client public key is invalid".to_owned()))?;
        let session_key = verifier
            .verify_client(client_proof)
            .map_err(|_| Error::Pairing("SRP client proof is invalid".to_owned()))?;
        let session_key: [u8; SRP_PROOF_LENGTH] = session_key
            .try_into()
            .map_err(|_| Error::Crypto("SRP session key has an unexpected length"))?;
        let server_proof: [u8; SRP_PROOF_LENGTH] = verifier
            .proof()
            .try_into()
            .map_err(|_| Error::Crypto("SRP server proof has an unexpected length"))?;
        Ok(SrpVerification {
            session_key: Zeroizing::new(session_key),
            server_proof,
        })
    }
}

pub(crate) fn left_pad<const LENGTH: usize>(input: &[u8]) -> Result<[u8; LENGTH]> {
    if input.len() > LENGTH {
        return Err(Error::Crypto("SRP group element exceeds its wire length"));
    }
    let mut padded = [0; LENGTH];
    padded[LENGTH - input.len()..].copy_from_slice(input);
    Ok(padded)
}

#[cfg(test)]
mod tests {
    use rand_core::{CryptoRng, Error as RngError, RngCore};

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

    #[test]
    fn completes_the_livi_srp_3072_sha512_variant() {
        let server = SrpServerSession::start(b"3939", &mut FixedRng(1)).expect("server");
        assert_eq!(server.salt().len(), 16);
        assert_eq!(server.public_key().len(), 384);
        use sha2_11::Digest as _;
        assert_eq!(
            hex::encode(sha2_11::Sha256::digest(server.public_key())),
            "4c12750ef4d8ae4353c3fac630a04fa52f617c4d5ee772304a5fd55e7c957db2"
        );

        let public = hex::decode(LIVI_CLIENT_PUBLIC_HEX).expect("client public fixture");
        let proof = hex::decode(LIVI_CLIENT_PROOF_HEX).expect("client proof fixture");
        let result = server.verify(&public, &proof).expect("server verification");
        assert_eq!(hex::encode(result.server_proof), LIVI_SERVER_PROOF_HEX);
        assert_eq!(
            hex::encode(result.session_key.as_ref()),
            LIVI_SESSION_KEY_HEX
        );
    }

    #[test]
    fn zero_public_key_and_bad_proof_are_rejected() {
        let server = SrpServerSession::start(b"3939", &mut FixedRng(1)).expect("server");
        assert!(matches!(
            server.verify(&[0; SRP_PUBLIC_KEY_LENGTH], &[0; SRP_PROOF_LENGTH]),
            Err(Error::Pairing(_))
        ));
    }
}
