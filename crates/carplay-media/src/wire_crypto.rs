use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit,
    aead::{Aead, Payload},
};

use crate::{Error, Result};

pub(crate) const TAG_LENGTH: usize = 16;

pub(crate) fn nonce(counter: u64) -> [u8; 12] {
    let mut nonce = [0; 12];
    nonce[4..].copy_from_slice(&counter.to_le_bytes());
    nonce
}

pub(crate) fn open(key: &[u8; 32], nonce: &[u8; 12], sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    ChaCha20Poly1305::new(key.into())
        .decrypt(nonce.into(), Payload { msg: sealed, aad })
        .map_err(|_| Error::Crypto("ChaCha20-Poly1305 authentication failed"))
}

pub(crate) fn seal(
    key: &[u8; 32],
    nonce: &[u8; 12],
    plaintext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>> {
    ChaCha20Poly1305::new(key.into())
        .encrypt(
            nonce.into(),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| Error::Crypto("ChaCha20-Poly1305 sealing failed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn airplay_nonce_is_zero_prefix_plus_little_endian_counter() {
        assert_eq!(
            nonce(0x0807_0605_0403_0201),
            [0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]
        );
    }
}
