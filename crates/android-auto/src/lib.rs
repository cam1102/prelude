#![forbid(unsafe_code)]

mod aoap;
mod backend;
mod error;
mod transport;
mod types;

pub use aoap::{discover, discover_with_vendor_ids, open_accessory, run_handshake};
pub use backend::{RusbBackend, RusbHandle, UsbBackend, UsbHandle};
pub use error::{Error, Result};
pub use transport::{AoapIo, BulkEndpoints, UsbAoapTransport, select_bulk_endpoints};
pub use types::{
    ACCESSORY_PRODUCT_IDS, ANDROID_PHONE_VENDOR_IDS, APPLE_VENDOR_ID, AccessoryIdentity,
    AoapCandidate, AoapMode, AoapTimeouts, CancellationToken, ConfigurationDescriptor,
    EndpointDescriptor, GET_PROTOCOL_REQUEST, GOOGLE_VENDOR_ID, InterfaceDescriptor,
    SEND_STRING_REQUEST, START_ACCESSORY_REQUEST, STRING_DESCRIPTION, STRING_MANUFACTURER,
    STRING_MODEL, STRING_SERIAL, STRING_URI, STRING_VERSION, TransferKind, UsbDeviceSnapshot,
    UsbTopology, is_accessory_id, is_probe_candidate, is_probe_candidate_with_vendor_ids,
};
