use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub const APPLE_VENDOR_ID: u16 = 0x05ac;
pub const GOOGLE_VENDOR_ID: u16 = 0x18d1;
pub const ACCESSORY_PRODUCT_IDS: [u16; 6] = [0x2d00, 0x2d01, 0x2d02, 0x2d03, 0x2d04, 0x2d05];
pub const ANDROID_PHONE_VENDOR_IDS: [u16; 22] = [
    0x18d1, 0x04e8, 0x22b8, 0x0fce, 0x12d1, 0x2717, 0x2a70, 0x0bb4, 0x1004, 0x19d2, 0x109b, 0x0e8d,
    0x17ef, 0x04dd, 0x0489, 0x0b05, 0x2d95, 0x2e04, 0x22d9, 0x2ae5, 0x1bbb, 0x2a45,
];

pub const GET_PROTOCOL_REQUEST: u8 = 51;
pub const SEND_STRING_REQUEST: u8 = 52;
pub const START_ACCESSORY_REQUEST: u8 = 53;

pub const STRING_MANUFACTURER: u16 = 0;
pub const STRING_MODEL: u16 = 1;
pub const STRING_DESCRIPTION: u16 = 2;
pub const STRING_VERSION: u16 = 3;
pub const STRING_URI: u16 = 4;
pub const STRING_SERIAL: u16 = 5;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UsbTopology {
    pub bus: u8,
    pub port_path: Vec<u8>,
}

impl fmt::Display for UsbTopology {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "bus {} port ", self.bus)?;
        for (index, port) in self.port_path.iter().enumerate() {
            if index > 0 {
                formatter.write_str(".")?;
            }
            write!(formatter, "{port}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferKind {
    Bulk,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointDescriptor {
    pub address: u8,
    pub transfer_kind: TransferKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceDescriptor {
    pub number: u8,
    pub alternate_setting: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub endpoints: Vec<EndpointDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigurationDescriptor {
    pub value: u8,
    pub interfaces: Vec<InterfaceDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDeviceSnapshot {
    pub topology: UsbTopology,
    pub address: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub device_class: u8,
    pub active_configuration: Option<u8>,
    pub configurations: Vec<ConfigurationDescriptor>,
}

impl UsbDeviceSnapshot {
    pub fn is_accessory_mode(&self) -> bool {
        is_accessory_id(self.vendor_id, self.product_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AoapMode {
    Normal { protocol_version: u16 },
    Accessory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AoapCandidate {
    pub device: UsbDeviceSnapshot,
    pub mode: AoapMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessoryIdentity {
    pub manufacturer: String,
    pub model: String,
    pub description: String,
    pub version: String,
    pub uri: String,
    pub serial: String,
}

impl Default for AccessoryIdentity {
    fn default() -> Self {
        Self {
            manufacturer: "Android".to_owned(),
            model: "Android Auto".to_owned(),
            description: "Prelude wired Android Auto host".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            uri: "https://github.com/68p/ct-carplay".to_owned(),
            serial: "prelude".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AoapTimeouts {
    pub control: Duration,
    pub reenumeration: Duration,
    pub poll_interval: Duration,
    pub transfer_poll_interval: Duration,
    pub retry_interval: Duration,
    pub open_attempts: usize,
    pub claim_attempts: usize,
}

impl Default for AoapTimeouts {
    fn default() -> Self {
        Self {
            control: Duration::from_secs(2),
            reenumeration: Duration::from_secs(5),
            poll_interval: Duration::from_millis(100),
            transfer_poll_interval: Duration::from_millis(100),
            retry_interval: Duration::from_millis(100),
            open_attempts: 5,
            claim_attempts: 5,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

pub const fn is_accessory_id(vendor_id: u16, product_id: u16) -> bool {
    if vendor_id != GOOGLE_VENDOR_ID {
        return false;
    }
    matches!(product_id, 0x2d00..=0x2d05)
}

pub fn is_probe_candidate(device: &UsbDeviceSnapshot) -> bool {
    is_probe_candidate_with_vendor_ids(device, &[])
}

pub fn is_probe_candidate_with_vendor_ids(
    device: &UsbDeviceSnapshot,
    additional_vendor_ids: &[u16],
) -> bool {
    const NON_PHONE_INTERFACE_CLASSES: [u8; 10] =
        [0x01, 0x02, 0x03, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0e, 0xe0];

    if device.vendor_id == APPLE_VENDOR_ID
        || (!ANDROID_PHONE_VENDOR_IDS.contains(&device.vendor_id)
            && !additional_vendor_ids.contains(&device.vendor_id))
        || !matches!(device.device_class, 0x00 | 0xff)
    {
        return false;
    }
    let interfaces = device
        .configurations
        .iter()
        .flat_map(|configuration| &configuration.interfaces)
        .collect::<Vec<_>>();
    !interfaces.is_empty()
        && interfaces
            .iter()
            .any(|interface| !NON_PHONE_INTERFACE_CLASSES.contains(&interface.class))
}
