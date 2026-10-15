#![forbid(unsafe_code)]

mod carkit;
mod config6;
mod discovery;
mod error;
mod lockdown;
mod mux;
mod ncm;
mod pairing;
mod tls;
mod wired_ncm;

pub use carkit::{
    CARKIT_SERVICE, CarkitByteStream, CarkitEvent, CarkitPolicy, CarkitSession, CarkitState,
    LockdownTrust, WiredCarrier, WiredCarrierConfig,
};
pub use config6::{
    Config6Activator, Config6Backend, Config6Policy, Config6Timeouts, LibUsbConfig6Backend,
};
pub use discovery::{
    APPLE_VENDOR_ID, ConfigurationPolicy, ConfigurationRequest, ConfiguredDevice, DeviceDiscovery,
    DeviceIdentity, EndpointInfo, IphoneCandidate, LibUsbConfigurator, LibUsbDiscovery, UsbAddress,
    UsbConfigurationController, UsbConfigurationInfo, UsbInterfaceInfo,
};
pub use error::{Error, Result};
pub use lockdown::LOCKDOWN_PORT;
pub use mux::{
    BulkTransport, MuxConnection, MuxHost, MuxPacket, MuxPacketCodec, RusbBulkTransport,
};
pub use ncm::{
    NCM_NDP16_SIGNATURE, NCM_NTH16_SIGNATURE, NcmBridgeEvent, NcmBridgeState, NcmFrame, Ntb16,
    Ntb16Encoder, NtbParameters, TapEndpoint,
};
pub use pairing::{CancellationToken, FirstTimePairingPolicy, PairRecord, PairRecordStore};
#[cfg(target_os = "linux")]
pub use wired_ncm::WiredNcm;
pub use wired_ncm::{NcmCarrierKind, WiredNcmConfig};
