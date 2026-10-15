use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

use crate::{Error, PairVerification, Result, crypto};

pub const MAXIMUM_CONTROL_FRAME_PAYLOAD: usize = 0x4000;
const AUTHENTICATION_TAG_LENGTH: usize = 16;
const LENGTH_HEADER_LENGTH: usize = 2;

pub struct ControlKeys {
    read_key: Zeroizing<[u8; 32]>,
    write_key: Zeroizing<[u8; 32]>,
}

pub struct EventChannelKeys {
    read_key: Zeroizing<[u8; 32]>,
    write_key: Zeroizing<[u8; 32]>,
}

impl std::fmt::Debug for EventChannelKeys {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EventChannelKeys")
            .field("read_key", &"[redacted]")
            .field("write_key", &"[redacted]")
            .finish()
    }
}

impl EventChannelKeys {
    pub fn new(read_key: [u8; 32], write_key: [u8; 32]) -> Result<Self> {
        if bool::from(read_key.ct_eq(&[0; 32])) || bool::from(write_key.ct_eq(&[0; 32])) {
            return Err(Error::Crypto("event-channel key is all zero"));
        }
        Ok(Self {
            read_key: Zeroizing::new(read_key),
            write_key: Zeroizing::new(write_key),
        })
    }

    fn into_control_keys(mut self) -> Result<ControlKeys> {
        let read_key = std::mem::take(&mut *self.read_key);
        let write_key = std::mem::take(&mut *self.write_key);
        ControlKeys::new(read_key, write_key)
    }
}

impl std::fmt::Debug for ControlKeys {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControlKeys")
            .field("read_key", &"[redacted]")
            .field("write_key", &"[redacted]")
            .finish()
    }
}

impl ControlKeys {
    pub fn new(read_key: [u8; 32], write_key: [u8; 32]) -> Result<Self> {
        if bool::from(read_key.ct_eq(&[0; 32])) || bool::from(write_key.ct_eq(&[0; 32])) {
            return Err(Error::Crypto("control-channel key is all zero"));
        }
        Ok(Self {
            read_key: Zeroizing::new(read_key),
            write_key: Zeroizing::new(write_key),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlCipherLimits {
    pub maximum_plaintext_bytes: usize,
    pub maximum_buffered_bytes: usize,
}

impl Default for ControlCipherLimits {
    fn default() -> Self {
        Self {
            maximum_plaintext_bytes: 4 * 1024 * 1024,
            maximum_buffered_bytes: 5 * 1024 * 1024,
        }
    }
}

impl ControlCipherLimits {
    fn validate(self) -> Result<Self> {
        let minimum_frame =
            LENGTH_HEADER_LENGTH + MAXIMUM_CONTROL_FRAME_PAYLOAD + AUTHENTICATION_TAG_LENGTH;
        if self.maximum_plaintext_bytes == 0 || self.maximum_buffered_bytes < minimum_frame {
            return Err(Error::ControlCipher("invalid control-cipher limits"));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlCipherState {
    Active,
    Failed,
}

pub struct ControlCipher {
    keys: ControlKeys,
    limits: ControlCipherLimits,
    read_counter: u64,
    write_counter: u64,
    encrypted_buffer: Zeroizing<Vec<u8>>,
    state: ControlCipherState,
}

pub struct EventChannelCipher(ControlCipher);

impl std::fmt::Debug for EventChannelCipher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("EventChannelCipher")
            .field(&self.0)
            .finish()
    }
}

impl EventChannelCipher {
    pub fn new(keys: EventChannelKeys) -> Result<Self> {
        Ok(Self(ControlCipher::new(keys.into_control_keys()?)?))
    }

    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        self.0.encrypt(plaintext)
    }

    pub fn decrypt(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        self.0.decrypt(input)
    }

    pub fn state(&self) -> ControlCipherState {
        self.0.state()
    }

    pub fn buffered_len(&self) -> usize {
        self.0.buffered_len()
    }
}

impl std::fmt::Debug for ControlCipher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControlCipher")
            .field("limits", &self.limits)
            .field("read_counter", &self.read_counter)
            .field("write_counter", &self.write_counter)
            .field("buffered_bytes", &self.encrypted_buffer.len())
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl ControlCipher {
    pub fn new(keys: ControlKeys) -> Result<Self> {
        Self::with_limits(keys, ControlCipherLimits::default())
    }

    pub fn from_verification(verification: &PairVerification) -> Result<Self> {
        Self::new(verification.control_keys()?)
    }

    pub fn with_limits(keys: ControlKeys, limits: ControlCipherLimits) -> Result<Self> {
        Ok(Self {
            keys,
            limits: limits.validate()?,
            read_counter: 0,
            write_counter: 0,
            encrypted_buffer: Zeroizing::new(Vec::new()),
            state: ControlCipherState::Active,
        })
    }

    pub fn state(&self) -> ControlCipherState {
        self.state
    }

    pub fn buffered_len(&self) -> usize {
        self.encrypted_buffer.len()
    }

    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        self.require_active()?;
        if plaintext.len() > self.limits.maximum_plaintext_bytes {
            return self.fail(Error::ControlCipher(
                "plaintext exceeds the configured message bound",
            ));
        }

        let frame_count = plaintext
            .len()
            .max(1)
            .div_ceil(MAXIMUM_CONTROL_FRAME_PAYLOAD);
        let overhead = frame_count
            .checked_mul(LENGTH_HEADER_LENGTH + AUTHENTICATION_TAG_LENGTH)
            .ok_or(Error::ControlCipher("encrypted message length overflow"))?;
        let capacity = plaintext
            .len()
            .checked_add(overhead)
            .ok_or(Error::ControlCipher("encrypted message length overflow"))?;
        let mut output = Vec::with_capacity(capacity);

        if plaintext.is_empty() {
            self.encrypt_chunk(&[], &mut output)?;
        } else {
            for chunk in plaintext.chunks(MAXIMUM_CONTROL_FRAME_PAYLOAD) {
                self.encrypt_chunk(chunk, &mut output)?;
            }
        }
        Ok(output)
    }

    pub fn decrypt(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        self.require_active()?;
        let new_length = self
            .encrypted_buffer
            .len()
            .checked_add(input.len())
            .ok_or(Error::ControlCipher("encrypted input length overflow"))?;
        if new_length > self.limits.maximum_buffered_bytes {
            return self.fail(Error::ControlCipher(
                "encrypted input exceeds the configured buffer bound",
            ));
        }
        self.encrypted_buffer.extend_from_slice(input);

        let mut plaintext = Zeroizing::new(Vec::new());
        let mut consumed = 0usize;
        while self.encrypted_buffer.len().saturating_sub(consumed) >= LENGTH_HEADER_LENGTH {
            let header = [
                self.encrypted_buffer[consumed],
                self.encrypted_buffer[consumed + 1],
            ];
            let payload_length = usize::from(u16::from_le_bytes(header));
            if payload_length > MAXIMUM_CONTROL_FRAME_PAYLOAD {
                return self.fail(Error::ControlCipher(
                    "frame payload exceeds the 16 KiB protocol maximum",
                ));
            }
            let frame_end = consumed
                .checked_add(LENGTH_HEADER_LENGTH)
                .and_then(|length| length.checked_add(payload_length))
                .and_then(|length| length.checked_add(AUTHENTICATION_TAG_LENGTH))
                .ok_or(Error::ControlCipher("encrypted frame length overflow"))?;
            if frame_end > self.encrypted_buffer.len() {
                break;
            }
            if self.read_counter == u64::MAX {
                return self.fail(Error::ControlCipher("read nonce space exhausted"));
            }
            let nonce = crypto::nonce_counter(self.read_counter);
            let decrypted = match crypto::open(
                &self.keys.read_key,
                &nonce,
                &self.encrypted_buffer[consumed + LENGTH_HEADER_LENGTH..frame_end],
                &header,
            ) {
                Ok(decrypted) => decrypted,
                Err(_) => {
                    return self.fail(Error::ControlCipher(
                        "frame authentication failed or a frame was replayed",
                    ));
                }
            };
            let new_plaintext_length = plaintext
                .len()
                .checked_add(decrypted.len())
                .ok_or(Error::ControlCipher("plaintext length overflow"))?;
            if new_plaintext_length > self.limits.maximum_plaintext_bytes {
                return self.fail(Error::ControlCipher(
                    "decrypted plaintext exceeds the configured message bound",
                ));
            }
            plaintext.extend_from_slice(&decrypted);
            self.read_counter += 1;
            consumed = frame_end;
        }
        if consumed > 0 {
            self.encrypted_buffer.drain(..consumed);
        }
        Ok(std::mem::take(&mut *plaintext))
    }

    fn encrypt_chunk(&mut self, chunk: &[u8], output: &mut Vec<u8>) -> Result<()> {
        if self.write_counter == u64::MAX {
            return self.fail(Error::ControlCipher("write nonce space exhausted"));
        }
        let length = u16::try_from(chunk.len())
            .map_err(|_| Error::ControlCipher("frame payload length overflow"))?;
        let header = length.to_le_bytes();
        let nonce = crypto::nonce_counter(self.write_counter);
        let sealed = crypto::seal(&self.keys.write_key, &nonce, chunk, &header)
            .map_err(|_| Error::ControlCipher("frame sealing failed"))?;
        output.extend_from_slice(&header);
        output.extend_from_slice(&sealed);
        self.write_counter += 1;
        Ok(())
    }

    fn require_active(&self) -> Result<()> {
        if self.state != ControlCipherState::Active {
            return Err(Error::NotReady {
                component: "encrypted control channel",
                state: "failed",
            });
        }
        Ok(())
    }

    fn fail<T>(&mut self, error: Error) -> Result<T> {
        self.state = ControlCipherState::Failed;
        self.keys.read_key.zeroize();
        self.keys.write_key.zeroize();
        self.encrypted_buffer.zeroize();
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (ControlCipher, ControlCipher) {
        (
            ControlCipher::new(ControlKeys::new([0xbb; 32], [0xaa; 32]).expect("phone keys"))
                .expect("phone"),
            ControlCipher::new(ControlKeys::new([0xaa; 32], [0xbb; 32]).expect("accessory keys"))
                .expect("accessory"),
        )
    }

    #[test]
    fn livi_framing_round_trips_and_splits_at_sixteen_kibibytes() {
        let (mut golden_phone, _) = pair();
        assert_eq!(
            hex::encode(golden_phone.encrypt(b"abc").expect("golden encrypt")),
            "0300807e9a7312519f7b6d64318d7ca529a9965a6d"
        );
        let (mut phone, mut accessory) = pair();
        let plaintext = vec![0x5a; MAXIMUM_CONTROL_FRAME_PAYLOAD + 100];
        let wire = phone.encrypt(&plaintext).expect("encrypt");
        assert_eq!(u16::from_le_bytes([wire[0], wire[1]]), 0x4000);
        let second = 2 + MAXIMUM_CONTROL_FRAME_PAYLOAD + AUTHENTICATION_TAG_LENGTH;
        assert_eq!(u16::from_le_bytes([wire[second], wire[second + 1]]), 100);
        assert_eq!(accessory.decrypt(&wire).expect("decrypt"), plaintext);
    }

    #[test]
    fn partial_frames_are_buffered_without_consuming_a_nonce() {
        let (mut phone, mut accessory) = pair();
        let wire = phone.encrypt(b"later").expect("encrypt");
        assert!(accessory.decrypt(&wire[..4]).expect("partial").is_empty());
        assert_eq!(accessory.buffered_len(), 4);
        assert_eq!(accessory.decrypt(&wire[4..]).expect("complete"), b"later");
    }

    #[test]
    fn replay_and_tampering_fail_closed() {
        let (mut phone, mut accessory) = pair();
        let wire = phone.encrypt(b"one").expect("encrypt");
        assert_eq!(accessory.decrypt(&wire).expect("first"), b"one");
        assert!(matches!(
            accessory.decrypt(&wire),
            Err(Error::ControlCipher(_))
        ));
        assert_eq!(accessory.state(), ControlCipherState::Failed);
        assert!(matches!(
            accessory.decrypt(&[]),
            Err(Error::NotReady { .. })
        ));
    }

    #[test]
    fn event_channel_is_encrypted_from_first_byte_and_rejects_tamper_and_replay() {
        let mut accessory = EventChannelCipher::new(
            EventChannelKeys::new([0x22; 32], [0x11; 32]).expect("accessory keys"),
        )
        .expect("accessory");
        let mut phone = EventChannelCipher::new(
            EventChannelKeys::new([0x11; 32], [0x22; 32]).expect("phone keys"),
        )
        .expect("phone");
        let plaintext = b"POST /command RTSP/1.0\r\nContent-Length: 0\r\n\r\n";
        let wire = accessory.encrypt(plaintext).expect("encrypt");
        assert_ne!(&wire[..2], &plaintext[..2]);
        assert_eq!(phone.decrypt(&wire).expect("decrypt"), plaintext);
        assert!(phone.decrypt(&wire).is_err(), "replay must fail");

        let mut accessory = EventChannelCipher::new(
            EventChannelKeys::new([0x22; 32], [0x11; 32]).expect("accessory keys"),
        )
        .expect("accessory");
        let mut phone = EventChannelCipher::new(
            EventChannelKeys::new([0x11; 32], [0x22; 32]).expect("phone keys"),
        )
        .expect("phone");
        let mut tampered = accessory.encrypt(plaintext).expect("encrypt");
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(phone.decrypt(&tampered).is_err(), "tamper must fail");
        assert_eq!(phone.state(), ControlCipherState::Failed);
    }

    #[test]
    fn oversized_frame_header_fails_before_allocation() {
        let (_, mut accessory) = pair();
        let invalid = u16::try_from(MAXIMUM_CONTROL_FRAME_PAYLOAD + 1)
            .expect("wire length")
            .to_le_bytes();
        assert!(matches!(
            accessory.decrypt(&invalid),
            Err(Error::ControlCipher(_))
        ));
        assert_eq!(accessory.state(), ControlCipherState::Failed);
    }
}
