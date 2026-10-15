use std::time::Duration;

use crate::TransportError;

mod mock;

#[cfg(target_os = "linux")]
mod linux;

pub use mock::{MockOperation, MockTransport, MockTransportState};

#[cfg(target_os = "linux")]
pub use linux::{LinuxI2c, LinuxI2cConfig};

pub trait Transport {
    fn select(&mut self, register: u8) -> std::result::Result<(), TransportError>;

    fn write(&mut self, register: u8, data: &[u8]) -> std::result::Result<(), TransportError>;

    fn read_exact(&mut self, out: &mut [u8]) -> std::result::Result<(), TransportError>;

    fn read_register(
        &mut self,
        register: u8,
        out: &mut [u8],
    ) -> std::result::Result<(), TransportError> {
        self.select(register)?;
        self.read_exact(out)
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}
