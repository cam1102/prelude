use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use super::Transport;
use crate::{
    CERTIFICATE_MAX_LEN, CERTIFICATE_MIN_LEN, CERTIFICATE_SERIAL_ASCII_LEN, CHALLENGE_LEN,
    RESPONSE_LEN, TransportError, registers as reg,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockOperation {
    Select(u8),
    Write(u8, Vec<u8>),
    Read(u8, usize),
    Sleep(Duration),
}

#[derive(Debug, Clone)]
pub struct MockTransportState {
    pub registers: BTreeMap<u8, Vec<u8>>,
    pub selected: Option<u8>,
    pub operations: Vec<MockOperation>,
    pub fail_next: Option<String>,
    pub status_after_trigger: u8,
    pub response_after_trigger: Vec<u8>,
}

impl Default for MockTransportState {
    fn default() -> Self {
        let certificate = vec![0xa5; CERTIFICATE_MIN_LEN];
        let mut registers = BTreeMap::new();
        registers.insert(reg::DEVICE_VERSION, vec![0x07]);
        registers.insert(reg::PROTOCOL_MAJOR, vec![0x03]);
        registers.insert(reg::PROTOCOL_MINOR, vec![0x00]);
        registers.insert(reg::ERROR_CODE, vec![0x00]);
        registers.insert(reg::AUTH_CONTROL_STATUS, vec![0x00]);
        registers.insert(reg::CHALLENGE_LENGTH, 0u16.to_be_bytes().to_vec());
        registers.insert(
            reg::CERTIFICATE_LENGTH,
            (certificate.len() as u16).to_be_bytes().to_vec(),
        );
        registers.insert(reg::CERTIFICATE_DATA, certificate);
        registers.insert(
            reg::CERTIFICATE_SERIAL,
            b"00112233445566778899AABBCCDDEEFF".to_vec(),
        );
        registers.insert(reg::RESPONSE_LENGTH, 0u16.to_be_bytes().to_vec());
        registers.insert(reg::RESPONSE_DATA, vec![0; RESPONSE_LEN]);
        Self {
            registers,
            selected: None,
            operations: Vec::new(),
            fail_next: None,
            status_after_trigger: reg::AUTHENTICATION_READY,
            response_after_trigger: vec![0x5a; RESPONSE_LEN],
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct MockTransport {
    state: Arc<Mutex<MockTransportState>>,
}

impl MockTransport {
    pub fn state(&self) -> Arc<Mutex<MockTransportState>> {
        Arc::clone(&self.state)
    }

    pub fn set_register(&self, register: u8, value: impl Into<Vec<u8>>) {
        self.state
            .lock()
            .expect("mock transport mutex poisoned")
            .registers
            .insert(register, value.into());
    }

    pub fn set_protocol_major(&self, major: u8) {
        self.set_register(reg::PROTOCOL_MAJOR, vec![major]);
    }

    pub fn set_certificate(&self, certificate: Vec<u8>) {
        assert!((CERTIFICATE_MIN_LEN..=CERTIFICATE_MAX_LEN).contains(&certificate.len()));
        let mut state = self.state.lock().expect("mock transport mutex poisoned");
        state.registers.insert(
            reg::CERTIFICATE_LENGTH,
            (certificate.len() as u16).to_be_bytes().to_vec(),
        );
        state.registers.insert(reg::CERTIFICATE_DATA, certificate);
    }

    pub fn set_certificate_serial_ascii(&self, serial: [u8; CERTIFICATE_SERIAL_ASCII_LEN]) {
        self.set_register(reg::CERTIFICATE_SERIAL, serial.to_vec());
    }

    pub fn set_sign_result(&self, status: u8, response: Vec<u8>) {
        let mut state = self.state.lock().expect("mock transport mutex poisoned");
        state.status_after_trigger = status;
        state.response_after_trigger = response;
    }

    pub fn fail_next(&self, message: impl Into<String>) {
        self.state
            .lock()
            .expect("mock transport mutex poisoned")
            .fail_next = Some(message.into());
    }

    fn maybe_fail(state: &mut MockTransportState) -> std::result::Result<(), TransportError> {
        match state.fail_next.take() {
            Some(message) => Err(TransportError::Protocol(message)),
            None => Ok(()),
        }
    }
}

impl Transport for MockTransport {
    fn select(&mut self, register: u8) -> std::result::Result<(), TransportError> {
        let mut state = self.state.lock().expect("mock transport mutex poisoned");
        Self::maybe_fail(&mut state)?;
        state.selected = Some(register);
        state.operations.push(MockOperation::Select(register));
        Ok(())
    }

    fn write(&mut self, register: u8, data: &[u8]) -> std::result::Result<(), TransportError> {
        let mut state = self.state.lock().expect("mock transport mutex poisoned");
        Self::maybe_fail(&mut state)?;
        state
            .operations
            .push(MockOperation::Write(register, data.to_vec()));
        state.registers.insert(register, data.to_vec());
        if register == reg::CHALLENGE_DATA {
            let length = if data.len() == CHALLENGE_LEN {
                CHALLENGE_LEN
            } else {
                0
            };
            state.registers.insert(
                reg::CHALLENGE_LENGTH,
                (length as u16).to_be_bytes().to_vec(),
            );
        } else if register == reg::AUTH_CONTROL_STATUS && data == [reg::START_AUTHENTICATION] {
            let status = state.status_after_trigger;
            let response = state.response_after_trigger.clone();
            state
                .registers
                .insert(reg::AUTH_CONTROL_STATUS, vec![status]);
            state.registers.insert(
                reg::RESPONSE_LENGTH,
                (response.len() as u16).to_be_bytes().to_vec(),
            );
            state.registers.insert(reg::RESPONSE_DATA, response);
        }
        Ok(())
    }

    fn read_exact(&mut self, out: &mut [u8]) -> std::result::Result<(), TransportError> {
        let mut state = self.state.lock().expect("mock transport mutex poisoned");
        Self::maybe_fail(&mut state)?;
        let register = state.selected.ok_or_else(|| {
            TransportError::Protocol("read attempted before register selection".into())
        })?;
        let data = state
            .registers
            .get(&register)
            .ok_or_else(|| {
                TransportError::Protocol(format!("mock register 0x{register:02x} is unset"))
            })?
            .clone();
        if data.len() < out.len() {
            return Err(TransportError::Protocol(format!(
                "mock register 0x{register:02x} has {} bytes, {} requested",
                data.len(),
                out.len()
            )));
        }
        out.copy_from_slice(&data[..out.len()]);
        state
            .operations
            .push(MockOperation::Read(register, out.len()));
        Ok(())
    }

    fn sleep(&mut self, duration: Duration) {
        self.state
            .lock()
            .expect("mock transport mutex poisoned")
            .operations
            .push(MockOperation::Sleep(duration));
    }
}
