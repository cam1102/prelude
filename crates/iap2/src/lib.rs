#![forbid(unsafe_code)]

pub mod auth;
pub mod csm;
pub mod frame;
pub mod session;

pub use auth::{AuthAction, AuthMachine, AuthProvider, AuthState};
pub use frame::{
    CONTROL_SESSION_ID, ControlBits, DETECT_MARKER, EXTERNAL_ACCESSORY_SESSION_ID,
    FILE_TRANSFER_SESSION_ID, LinkPacket, LinkPacketDecoder, LinkSynchronization,
    SessionDescriptor, SessionType,
};
pub use session::{
    CarkitIo, LinkProfile, SessionError, SessionPhase, SessionTimeouts, WiredCarPlayStartRequest,
    WiredCarkitSession, WirelessCarPlayStartRequest,
};
