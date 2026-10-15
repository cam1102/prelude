use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use sha2::Sha512;
use zeroize::Zeroizing;

use crate::{Error, Result};

pub(crate) fn hkdf_sha512(
    input_key_material: &[u8],
    salt: &[u8],
    info: &[u8],
    output: &mut [u8],
) -> Result<()> {
    Hkdf::<Sha512>::new(Some(salt), input_key_material)
        .expand(info, output)
        .map_err(|_| Error::Crypto("HKDF-SHA512 output length is invalid"))
}

pub(crate) fn nonce_label(label: &[u8]) -> Result<[u8; 12]> {
    if label.len() != 8 || !label.is_ascii() {
        return Err(Error::Crypto(
            "pairing nonce label must be exactly eight ASCII bytes",
        ));
    }
    let mut nonce = [0; 12];
    nonce[4..].copy_from_slice(label);
    Ok(nonce)
}

pub(crate) fn nonce_counter(counter: u64) -> [u8; 12] {
    let mut nonce = [0; 12];
    nonce[4..].copy_from_slice(&counter.to_le_bytes());
    nonce
}

pub(crate) fn seal(
    key: &[u8; 32],
    nonce: &[u8; 12],
    plaintext: &[u8],
    associated_data: &[u8],
) -> Result<Vec<u8>> {
    ChaCha20Poly1305::new(Key::from_slice(key))
        .encrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: plaintext,
                aad: associated_data,
            },
        )
        .map_err(|_| Error::Crypto("ChaCha20-Poly1305 sealing failed"))
}

pub(crate) fn open(
    key: &[u8; 32],
    nonce: &[u8; 12],
    ciphertext_and_tag: &[u8],
    associated_data: &[u8],
) -> Result<Zeroizing<Vec<u8>>> {
    ChaCha20Poly1305::new(Key::from_slice(key))
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ciphertext_and_tag,
                aad: associated_data,
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| Error::Crypto("ChaCha20-Poly1305 authentication failed"))
}

pub(crate) fn sign(signing_key: &SigningKey, message: &[u8]) -> [u8; 64] {
    signing_key.sign(message).to_bytes()
}

pub(crate) fn verify(public_key: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
    let Ok(verifying_key) = VerifyingKey::from_bytes(public_key) else {
        return false;
    };
    verifying_key
        .verify_strict(message, &Signature::from_bytes(signature))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn livi_nonce_layouts_are_exact() {
        assert_eq!(
            nonce_label(b"PV-Msg02").expect("label"),
            [0, 0, 0, 0, b'P', b'V', b'-', b'M', b's', b'g', b'0', b'2']
        );
        assert_eq!(
            nonce_counter(0x0102_0304_0506_0708),
            [0, 0, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1]
        );
    }
}
