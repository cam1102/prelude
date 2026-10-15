use std::sync::{Arc, Mutex, MutexGuard};

use prelude_iap2::AuthProvider;
use prelude_mfi::{CERTIFICATE_SERIAL_LEN, CHALLENGE_LEN, LinuxI2c, MfiAuth, RESPONSE_LEN};
use prelude_net::{MfiSigner, SignerError};
use thiserror::Error;

#[derive(Clone)]
pub(crate) struct SharedMfi {
    inner: Arc<Mutex<MfiAuth<LinuxI2c>>>,
    protocol_major: u8,
}

impl std::fmt::Debug for SharedMfi {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SharedMfi")
            .field("protocol_major", &self.protocol_major)
            .field("shared_hardware", &true)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Error)]
pub(crate) enum SharedMfiError {
    #[error("the shared MFi hardware lock is poisoned")]
    Poisoned,
    #[error("MFi coprocessor operation failed: {0}")]
    Hardware(#[from] prelude_mfi::Error),
}

impl SharedMfi {
    pub(crate) fn open_default() -> Result<Self, SharedMfiError> {
        Self::from_auth(MfiAuth::open_default()?)
    }

    fn from_auth(auth: MfiAuth<LinuxI2c>) -> Result<Self, SharedMfiError> {
        let protocol_major = auth.device_info().protocol_major;
        if protocol_major != prelude_mfi::REQUIRED_PROTOCOL_MAJOR {
            return Err(SharedMfiError::Hardware(
                prelude_mfi::Error::UnsupportedProtocolMajor {
                    expected: prelude_mfi::REQUIRED_PROTOCOL_MAJOR,
                    actual: protocol_major,
                },
            ));
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(auth)),
            protocol_major,
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, MfiAuth<LinuxI2c>>, SharedMfiError> {
        self.inner.lock().map_err(|_| SharedMfiError::Poisoned)
    }
}

impl AuthProvider for SharedMfi {
    type Error = SharedMfiError;

    fn certificate(&mut self) -> Result<Vec<u8>, Self::Error> {
        Ok(self.lock()?.certificate()?)
    }

    fn certificate_serial(&mut self) -> Result<[u8; CERTIFICATE_SERIAL_LEN], Self::Error> {
        Ok(self.lock()?.certificate_serial()?)
    }

    fn sign(&mut self, challenge: &[u8; CHALLENGE_LEN]) -> Result<[u8; RESPONSE_LEN], Self::Error> {
        Ok(self.lock()?.sign(challenge)?)
    }
}

impl MfiSigner for SharedMfi {
    fn protocol_major(&self) -> u8 {
        self.protocol_major
    }

    fn certificate(&mut self) -> Result<Vec<u8>, SignerError> {
        self.lock()
            .and_then(|mut auth| auth.certificate().map_err(SharedMfiError::from))
            .map_err(|error| SignerError::new(error.to_string()))
    }

    fn sign_digest(&mut self, digest: &[u8; 32]) -> Result<Vec<u8>, SignerError> {
        self.lock()
            .and_then(|mut auth| auth.sign(digest).map_err(SharedMfiError::from))
            .map(|signature| signature.to_vec())
            .map_err(|error| SignerError::new(error.to_string()))
    }
}
