use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU16, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use rusb::{DeviceHandle, Direction, GlobalContext, TransferType};

use crate::{Error, IphoneCandidate, Result, discovery::normalize_apple_usb_serial};

const MUX_OUTGOING_MAGIC: u32 = 0xfeed_face;
const MUX_INCOMING_MAGIC: u32 = 0xface_face;
const PROTOCOL_VERSION: u32 = 0;
const PROTOCOL_SETUP: u32 = 2;
const PROTOCOL_TCP: u32 = 6;
const TCP_FIN: u8 = 0x01;
const TCP_SYN: u8 = 0x02;
const TCP_RST: u8 = 0x04;
const TCP_ACK: u8 = 0x10;
const TCP_WINDOW: u16 = 512;
const TCP_HEADER_LEN: usize = 20;
const MUX_HEADER_LEN: usize = 16;
const MAX_TCP_PAYLOAD: usize = 16_384;
const MAX_MUX_PACKET: usize = 1024 * 1024;
const MAX_STALE_HANDSHAKE_FRAMES: usize = 32;
const CONFIGURATION: u8 = 6;
const MUX_INTERFACE: u8 = 1;

pub trait BulkTransport: std::fmt::Debug + Send + Sync + 'static {
    fn write(&self, bytes: &[u8], timeout: Duration) -> Result<()>;
    fn read(&self, maximum: usize, timeout: Duration) -> Result<Vec<u8>>;
}

pub struct RusbBulkTransport {
    handle: DeviceHandle<GlobalContext>,
    interface_number: u8,
    endpoint_in: u8,
    endpoint_out: u8,
}

impl std::fmt::Debug for RusbBulkTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RusbBulkTransport")
            .field("interface_number", &self.interface_number)
            .field("endpoint_in", &format_args!("0x{:02x}", self.endpoint_in))
            .field("endpoint_out", &format_args!("0x{:02x}", self.endpoint_out))
            .finish_non_exhaustive()
    }
}

impl RusbBulkTransport {
    pub fn open(candidate: &IphoneCandidate) -> Result<Self> {
        if candidate.active_configuration != Some(CONFIGURATION) {
            return Err(Error::NotReady {
                component: "usbmux USB transport",
                state: "configuration_6_inactive",
            });
        }
        let devices = rusb::devices()?;
        let device = devices
            .iter()
            .find(|device| {
                device.bus_number() == candidate.identity.address.bus
                    && device.address() == candidate.identity.address.address
            })
            .ok_or(Error::DeviceNotFound {
                bus: candidate.identity.address.bus,
                address: candidate.identity.address.address,
            })?;
        let descriptor = device.device_descriptor()?;
        if descriptor.vendor_id() != candidate.identity.vendor_id
            || descriptor.product_id() != candidate.identity.product_id
        {
            return Err(Error::IdentityChanged {
                stage: "opening the configuration-6 usbmux transport",
            });
        }
        let handle = device.open()?;
        if let Some(expected) = candidate.identity.serial_number.as_deref() {
            let observed = handle.read_serial_number_string_ascii(&descriptor)?;
            if normalize_apple_usb_serial(&observed)? != expected {
                return Err(Error::IdentityChanged {
                    stage: "validating the configuration-6 USB serial",
                });
            }
        }
        match handle.set_auto_detach_kernel_driver(true) {
            Ok(()) | Err(rusb::Error::NotSupported) => {}
            Err(error) => return Err(error.into()),
        }

        let mut configuration = None;
        for index in 0..descriptor.num_configurations() {
            let candidate_configuration = device.config_descriptor(index)?;
            if candidate_configuration.number() == CONFIGURATION {
                configuration = Some(candidate_configuration);
                break;
            }
        }
        let configuration = configuration.ok_or_else(|| {
            Error::InvalidDescriptor("USB configuration 6 disappeared before claim".to_owned())
        })?;
        let mut endpoints = None;
        for interface in configuration.interfaces() {
            if interface.number() != MUX_INTERFACE {
                continue;
            }
            for descriptor in interface.descriptors() {
                let mut endpoint_in = None;
                let mut endpoint_out = None;
                for endpoint in descriptor.endpoint_descriptors() {
                    if endpoint.transfer_type() != TransferType::Bulk {
                        continue;
                    }
                    match endpoint.direction() {
                        Direction::In => endpoint_in = Some(endpoint.address()),
                        Direction::Out => endpoint_out = Some(endpoint.address()),
                    }
                }
                if let (Some(endpoint_in), Some(endpoint_out)) = (endpoint_in, endpoint_out) {
                    endpoints = Some((descriptor.setting_number(), endpoint_in, endpoint_out));
                    break;
                }
            }
        }
        let (alternate_setting, endpoint_in, endpoint_out) = endpoints.ok_or_else(|| {
            Error::InvalidDescriptor(
                "configuration 6 interface 1 has no bulk IN/OUT pair".to_owned(),
            )
        })?;
        handle.claim_interface(MUX_INTERFACE)?;
        if alternate_setting != 0 {
            handle.set_alternate_setting(MUX_INTERFACE, alternate_setting)?;
        }
        Ok(Self {
            handle,
            interface_number: MUX_INTERFACE,
            endpoint_in,
            endpoint_out,
        })
    }

    pub fn interface_number(&self) -> u8 {
        self.interface_number
    }
}

impl BulkTransport for RusbBulkTransport {
    fn write(&self, bytes: &[u8], timeout: Duration) -> Result<()> {
        let mut offset = 0;
        while offset < bytes.len() {
            let written = match self
                .handle
                .write_bulk(self.endpoint_out, &bytes[offset..], timeout)
            {
                Ok(written) => written,
                Err(rusb::Error::Timeout) => {
                    return Err(Error::Timeout {
                        operation: "writing usbmux bulk endpoint",
                        timeout,
                    });
                }
                Err(rusb::Error::NoDevice) => return Err(Error::Disconnected),
                Err(error) => return Err(error.into()),
            };
            if written == 0 {
                return Err(Error::Disconnected);
            }
            offset += written;
        }
        Ok(())
    }

    fn read(&self, maximum: usize, timeout: Duration) -> Result<Vec<u8>> {
        let mut bytes = vec![0; maximum];
        match self.handle.read_bulk(self.endpoint_in, &mut bytes, timeout) {
            Ok(read) => {
                bytes.truncate(read);
                Ok(bytes)
            }
            Err(rusb::Error::Timeout) => Err(Error::Timeout {
                operation: "reading usbmux bulk endpoint",
                timeout,
            }),
            Err(rusb::Error::NoDevice) => Err(Error::Disconnected),
            Err(error) => Err(error.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuxPacket {
    pub protocol: u32,
    pub transmit_sequence: u16,
    pub receive_sequence: u16,
    pub payload: Vec<u8>,
}

impl MuxPacket {
    fn encode(&self) -> Result<Vec<u8>> {
        self.encode_with_magic(MUX_OUTGOING_MAGIC)
    }

    fn encode_with_magic(&self, magic: u32) -> Result<Vec<u8>> {
        let length = MUX_HEADER_LEN
            .checked_add(self.payload.len())
            .ok_or_else(|| Error::InvalidMux("packet length overflow".to_owned()))?;
        let length = u32::try_from(length)
            .map_err(|_| Error::InvalidMux("packet exceeds 32-bit length".to_owned()))?;
        let mut encoded = Vec::with_capacity(length as usize);
        encoded.extend_from_slice(&self.protocol.to_be_bytes());
        encoded.extend_from_slice(&length.to_be_bytes());
        encoded.extend_from_slice(&magic.to_be_bytes());
        encoded.extend_from_slice(&self.transmit_sequence.to_be_bytes());
        encoded.extend_from_slice(&self.receive_sequence.to_be_bytes());
        encoded.extend_from_slice(&self.payload);
        Ok(encoded)
    }
}

#[derive(Debug, Default)]
pub struct MuxPacketCodec {
    buffer: Vec<u8>,
}

impl MuxPacketCodec {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<MuxPacket>> {
        if self.buffer.len().saturating_add(bytes.len()) > MAX_MUX_PACKET * 2 {
            return Err(Error::InvalidMux(
                "receive buffer limit exceeded".to_owned(),
            ));
        }
        self.buffer.extend_from_slice(bytes);
        let mut packets = Vec::new();
        loop {
            if self.buffer.len() < MUX_HEADER_LEN {
                break;
            }
            let protocol = read_u32(&self.buffer, 0)?;
            let length = usize::try_from(read_u32(&self.buffer, 4)?)
                .map_err(|_| Error::InvalidMux("packet length is not representable".to_owned()))?;
            if !(MUX_HEADER_LEN..=MAX_MUX_PACKET).contains(&length) {
                return Err(Error::InvalidMux(format!(
                    "declared packet length {length} is outside {MUX_HEADER_LEN}..={MAX_MUX_PACKET}"
                )));
            }
            if read_u32(&self.buffer, 8)? != MUX_INCOMING_MAGIC {
                return Err(Error::InvalidMux("invalid mux magic".to_owned()));
            }
            if self.buffer.len() < length {
                break;
            }
            packets.push(MuxPacket {
                protocol,
                transmit_sequence: read_u16(&self.buffer, 12)?,
                receive_sequence: read_u16(&self.buffer, 14)?,
                payload: self.buffer[MUX_HEADER_LEN..length].to_vec(),
            });
            self.buffer.drain(..length);
        }
        Ok(packets)
    }
}

#[derive(Debug)]
enum MuxEvent {
    Tcp(TcpPacket),
    Failed(String),
}

#[derive(Debug)]
struct TcpPacket {
    sequence: u32,
    acknowledgement: u32,
    flags: u8,
    window: u16,
    payload: Vec<u8>,
}

struct HostInner<B> {
    transport: B,
    transmit_sequence: AtomicU16,
    receive_sequence: AtomicU16,
    next_source_port: AtomicU16,
    connections: Mutex<HashMap<u16, Sender<MuxEvent>>>,
    alive: AtomicBool,
    io_timeout: Duration,
}

impl<B> std::fmt::Debug for HostInner<B> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostInner")
            .field("alive", &self.alive.load(Ordering::Relaxed))
            .field("io_timeout", &self.io_timeout)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct MuxHost<B> {
    inner: Arc<HostInner<B>>,
}

impl<B> Clone for MuxHost<B> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<B: BulkTransport> MuxHost<B> {
    pub fn start(transport: B, handshake_timeout: Duration, io_timeout: Duration) -> Result<Self> {
        if handshake_timeout.is_zero() || io_timeout.is_zero() {
            return Err(Error::InvalidMux(
                "mux timeouts must be non-zero".to_owned(),
            ));
        }
        let mut version = Vec::with_capacity(20);
        version.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        version.extend_from_slice(&20_u32.to_be_bytes());
        version.extend_from_slice(&2_u32.to_be_bytes());
        version.extend_from_slice(&0_u32.to_be_bytes());
        version.extend_from_slice(&0_u32.to_be_bytes());
        transport.write(&version, handshake_timeout)?;
        let response = read_version_response(&transport, handshake_timeout)?;
        validate_version_response(&response)?;

        let inner = Arc::new(HostInner {
            transport,
            transmit_sequence: AtomicU16::new(0),
            receive_sequence: AtomicU16::new(0),
            next_source_port: AtomicU16::new(1),
            connections: Mutex::new(HashMap::new()),
            alive: AtomicBool::new(true),
            io_timeout,
        });
        send_mux_packet(&inner, PROTOCOL_SETUP, &[7])?;
        start_reader(Arc::downgrade(&inner));
        Ok(Self { inner })
    }

    pub fn is_alive(&self) -> bool {
        self.inner.alive.load(Ordering::Acquire)
    }

    pub fn connect(&self, destination_port: u16, timeout: Duration) -> Result<MuxConnection<B>> {
        if !self.is_alive() {
            return Err(Error::Disconnected);
        }
        let source_port = self.inner.next_source_port.fetch_add(1, Ordering::Relaxed);
        if source_port == 0 {
            return Err(Error::InvalidMux("source port space exhausted".to_owned()));
        }
        let (sender, receiver) = mpsc::channel();
        self.inner
            .connections
            .lock()
            .map_err(|_| Error::InvalidMux("connection registry lock poisoned".to_owned()))?
            .insert(source_port, sender);
        let mut connection = MuxConnection {
            host: self.clone(),
            source_port,
            destination_port,
            local_sequence: 0,
            remote_sequence: 0,
            receiver,
            pending: VecDeque::new(),
            read_timeout: timeout,
            write_timeout: timeout,
            default_read_timeout: timeout,
            default_write_timeout: timeout,
            closed: false,
        };
        connection.send_tcp(TCP_SYN, &[])?;
        match connection.receiver.recv_timeout(timeout) {
            Ok(MuxEvent::Tcp(packet))
                if packet.flags & (TCP_SYN | TCP_ACK) == (TCP_SYN | TCP_ACK)
                    && packet.acknowledgement == 1 =>
            {
                connection.local_sequence = 1;
                connection.remote_sequence = packet.sequence.wrapping_add(1);
                connection.send_tcp(TCP_ACK, &[])?;
                Ok(connection)
            }
            Ok(MuxEvent::Tcp(packet)) => Err(Error::InvalidMux(format!(
                "unexpected connect response flags=0x{:02x} ack={} window={}",
                packet.flags, packet.acknowledgement, packet.window
            ))),
            Ok(MuxEvent::Failed(reason)) => Err(Error::InvalidMux(reason)),
            Err(RecvTimeoutError::Timeout) => Err(Error::Timeout {
                operation: "connecting usbmux TCP stream",
                timeout,
            }),
            Err(RecvTimeoutError::Disconnected) => Err(Error::Disconnected),
        }
    }
}

pub struct MuxConnection<B: BulkTransport> {
    host: MuxHost<B>,
    source_port: u16,
    destination_port: u16,
    local_sequence: u32,
    remote_sequence: u32,
    receiver: Receiver<MuxEvent>,
    pending: VecDeque<u8>,
    read_timeout: Duration,
    write_timeout: Duration,
    default_read_timeout: Duration,
    default_write_timeout: Duration,
    closed: bool,
}

impl<B: BulkTransport> std::fmt::Debug for MuxConnection<B> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MuxConnection")
            .field("source_port", &self.source_port)
            .field("destination_port", &self.destination_port)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl<B: BulkTransport> MuxConnection<B> {
    pub fn set_read_timeout(&mut self, timeout: Duration) -> Result<()> {
        if timeout.is_zero() {
            return Err(Error::InvalidMux(
                "usbmux TCP read timeout must be non-zero".to_owned(),
            ));
        }
        self.read_timeout = timeout;
        Ok(())
    }

    pub fn set_write_timeout(&mut self, timeout: Duration) -> Result<()> {
        if timeout.is_zero() {
            return Err(Error::InvalidMux(
                "usbmux TCP write timeout must be non-zero".to_owned(),
            ));
        }
        self.write_timeout = timeout;
        Ok(())
    }

    pub fn read_timeout(&self) -> Duration {
        self.read_timeout
    }

    pub fn write_timeout(&self) -> Duration {
        self.write_timeout
    }

    pub fn reset_read_timeout(&mut self) {
        self.read_timeout = self.default_read_timeout;
    }

    pub fn reset_write_timeout(&mut self) {
        self.write_timeout = self.default_write_timeout;
    }

    pub fn is_alive(&self) -> bool {
        !self.closed && self.host.is_alive()
    }

    pub fn close(&mut self) -> Result<()> {
        if !self.closed && self.host.is_alive() {
            self.send_tcp(TCP_FIN | TCP_ACK, &[])?;
            self.local_sequence = self.local_sequence.wrapping_add(1);
        }
        self.closed = true;
        if let Ok(mut connections) = self.host.inner.connections.lock() {
            connections.remove(&self.source_port);
        }
        Ok(())
    }

    fn read_carrier(&mut self, output: &mut [u8]) -> Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            if !self.pending.is_empty() {
                let count = output.len().min(self.pending.len());
                for byte in &mut output[..count] {
                    *byte = self
                        .pending
                        .pop_front()
                        .ok_or_else(|| Error::InvalidMux("pending payload underflow".to_owned()))?;
                }
                return Ok(count);
            }
            if self.closed || !self.host.is_alive() {
                return Ok(0);
            }
            match self.receiver.recv_timeout(self.read_timeout) {
                Ok(MuxEvent::Tcp(packet)) => {
                    if packet.flags & TCP_RST != 0 {
                        self.closed = true;
                        return Err(Error::Disconnected);
                    }
                    if !packet.payload.is_empty() {
                        if packet.sequence != self.remote_sequence {
                            return Err(Error::InvalidMux(format!(
                                "out-of-order TCP payload: received {}, expected {}",
                                packet.sequence, self.remote_sequence
                            )));
                        }
                        self.remote_sequence = self.remote_sequence.wrapping_add(
                            u32::try_from(packet.payload.len()).map_err(|_| {
                                Error::InvalidMux("TCP payload length overflow".to_owned())
                            })?,
                        );
                        self.pending.extend(packet.payload);
                        self.send_tcp(TCP_ACK, &[])?;
                    }
                    if packet.flags & TCP_FIN != 0 {
                        self.remote_sequence = self.remote_sequence.wrapping_add(1);
                        self.send_tcp(TCP_ACK, &[])?;
                        self.closed = true;
                    }
                }
                Ok(MuxEvent::Failed(reason)) => return Err(Error::InvalidMux(reason)),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(Error::Timeout {
                        operation: "reading usbmux TCP stream",
                        timeout: self.read_timeout,
                    });
                }
                Err(RecvTimeoutError::Disconnected) => return Err(Error::Disconnected),
            }
        }
    }

    fn write_carrier(&mut self, bytes: &[u8]) -> Result<()> {
        if self.closed || !self.host.is_alive() {
            return Err(Error::Disconnected);
        }
        for chunk in bytes.chunks(MAX_TCP_PAYLOAD) {
            self.send_tcp(TCP_ACK, chunk)?;
            self.local_sequence =
                self.local_sequence
                    .wrapping_add(u32::try_from(chunk.len()).map_err(|_| {
                        Error::InvalidMux("TCP payload length overflow".to_owned())
                    })?);
        }
        Ok(())
    }

    fn send_tcp(&self, flags: u8, payload: &[u8]) -> Result<()> {
        let mut packet = Vec::with_capacity(TCP_HEADER_LEN + payload.len());
        packet.extend_from_slice(&self.source_port.to_be_bytes());
        packet.extend_from_slice(&self.destination_port.to_be_bytes());
        packet.extend_from_slice(&self.local_sequence.to_be_bytes());
        packet.extend_from_slice(&self.remote_sequence.to_be_bytes());
        packet.push(0x50);
        packet.push(flags);
        packet.extend_from_slice(&TCP_WINDOW.to_be_bytes());
        packet.extend_from_slice(&0_u16.to_be_bytes());
        packet.extend_from_slice(&0_u16.to_be_bytes());
        packet.extend_from_slice(payload);
        send_mux_packet_with_timeout(&self.host.inner, PROTOCOL_TCP, &packet, self.write_timeout)
    }
}

impl<B: BulkTransport> Read for MuxConnection<B> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        self.read_carrier(output).map_err(carrier_io_error)
    }
}

impl<B: BulkTransport> Write for MuxConnection<B> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.write_carrier(bytes)
            .map(|()| bytes.len())
            .map_err(carrier_io_error)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<B: BulkTransport> Drop for MuxConnection<B> {
    fn drop(&mut self) {
        if !self.closed && self.host.is_alive() {
            let _ = self.send_tcp(TCP_FIN | TCP_ACK, &[]);
        }
        self.closed = true;
        if let Ok(mut connections) = self.host.inner.connections.lock() {
            connections.remove(&self.source_port);
        }
    }
}

fn start_reader<B: BulkTransport>(host: Weak<HostInner<B>>) {
    thread::spawn(move || {
        let mut codec = MuxPacketCodec::default();
        loop {
            let Some(inner) = host.upgrade() else {
                return;
            };
            if !inner.alive.load(Ordering::Acquire) {
                return;
            }
            let read = inner.transport.read(64 * 1024, inner.io_timeout);
            let bytes = match read {
                Ok(bytes) => bytes,
                Err(Error::Timeout { .. }) => continue,
                Err(error) => {
                    fail_host(&inner, error.to_string());
                    return;
                }
            };
            if bytes.is_empty() {
                continue;
            }
            let packets = match codec.push(&bytes) {
                Ok(packets) => packets,
                Err(error) => {
                    fail_host(&inner, error.to_string());
                    return;
                }
            };
            for packet in packets {
                inner
                    .receive_sequence
                    .store(packet.transmit_sequence, Ordering::Release);
                if packet.protocol != PROTOCOL_TCP {
                    continue;
                }
                let tcp = match parse_tcp(&packet.payload) {
                    Ok(tcp) => tcp,
                    Err(error) => {
                        fail_host(&inner, error.to_string());
                        return;
                    }
                };
                let destination_port = match read_u16(&packet.payload, 2) {
                    Ok(port) => port,
                    Err(error) => {
                        fail_host(&inner, error.to_string());
                        return;
                    }
                };
                if let Some(connection) = inner
                    .connections
                    .lock()
                    .ok()
                    .and_then(|connections| connections.get(&destination_port).cloned())
                {
                    let _ = connection.send(MuxEvent::Tcp(tcp));
                }
            }
        }
    });
}

fn send_mux_packet<B: BulkTransport>(
    inner: &HostInner<B>,
    protocol: u32,
    payload: &[u8],
) -> Result<()> {
    send_mux_packet_with_timeout(inner, protocol, payload, inner.io_timeout)
}

fn send_mux_packet_with_timeout<B: BulkTransport>(
    inner: &HostInner<B>,
    protocol: u32,
    payload: &[u8],
    timeout: Duration,
) -> Result<()> {
    if !inner.alive.load(Ordering::Acquire) {
        return Err(Error::Disconnected);
    }
    let transmit_sequence = inner.transmit_sequence.fetch_add(1, Ordering::AcqRel);
    let packet = MuxPacket {
        protocol,
        transmit_sequence,
        receive_sequence: inner.receive_sequence.load(Ordering::Acquire),
        payload: payload.to_vec(),
    }
    .encode()?;
    inner.transport.write(&packet, timeout)
}

fn carrier_io_error(error: Error) -> std::io::Error {
    let kind = match &error {
        Error::Timeout { .. } => std::io::ErrorKind::TimedOut,
        Error::Disconnected => std::io::ErrorKind::UnexpectedEof,
        Error::InvalidMux(_) => std::io::ErrorKind::InvalidData,
        _ => std::io::ErrorKind::Other,
    };
    std::io::Error::new(kind, error.to_string())
}

fn parse_tcp(payload: &[u8]) -> Result<TcpPacket> {
    if payload.len() < TCP_HEADER_LEN {
        return Err(Error::InvalidMux("truncated TCP header".to_owned()));
    }
    let data_offset = usize::from(payload[12] >> 4) * 4;
    if data_offset < TCP_HEADER_LEN || data_offset > payload.len() {
        return Err(Error::InvalidMux("invalid TCP data offset".to_owned()));
    }
    Ok(TcpPacket {
        sequence: read_u32(payload, 4)?,
        acknowledgement: read_u32(payload, 8)?,
        flags: payload[13],
        window: read_u16(payload, 14)?,
        payload: payload[data_offset..].to_vec(),
    })
}

fn fail_host<B>(inner: &HostInner<B>, reason: String) {
    inner.alive.store(false, Ordering::Release);
    if let Ok(connections) = inner.connections.lock() {
        for connection in connections.values() {
            let _ = connection.send(MuxEvent::Failed(reason.clone()));
        }
    }
}

fn validate_version_response(response: &[u8]) -> Result<()> {
    if response.len() < 8 {
        return Err(Error::InvalidMux("truncated version response".to_owned()));
    }
    if read_u32(response, 0)? != PROTOCOL_VERSION {
        return Err(Error::InvalidMux("unexpected version protocol".to_owned()));
    }
    let length = usize::try_from(read_u32(response, 4)?)
        .map_err(|_| Error::InvalidMux("version length is not representable".to_owned()))?;
    if length < 8 || length > response.len() {
        return Err(Error::InvalidMux(
            "invalid version response length".to_owned(),
        ));
    }
    Ok(())
}

fn read_version_response<B: BulkTransport>(
    transport: &B,
    handshake_timeout: Duration,
) -> Result<Vec<u8>> {
    let started = Instant::now();
    let mut response = Vec::new();
    let mut stale_frames = 0_usize;
    loop {
        while response.len() >= 8 {
            let protocol = read_u32(&response, 0)?;
            let length = usize::try_from(read_u32(&response, 4)?)
                .map_err(|_| Error::InvalidMux("version length is not representable".to_owned()))?;
            if !(8..=MAX_MUX_PACKET).contains(&length) {
                return Err(Error::InvalidMux(format!(
                    "version response length {length} is outside 8..={MAX_MUX_PACKET}"
                )));
            }
            if response.len() < length {
                break;
            }
            if protocol == PROTOCOL_VERSION {
                response.truncate(length);
                return Ok(response);
            }

            if length < MUX_HEADER_LEN {
                return Err(Error::InvalidMux(format!(
                    "stale handshake frame length {length} is shorter than the mux header"
                )));
            }
            if read_u32(&response, 8)? != MUX_INCOMING_MAGIC {
                return Err(Error::InvalidMux(
                    "stale handshake frame has invalid inbound magic".to_owned(),
                ));
            }
            stale_frames = stale_frames.saturating_add(1);
            if stale_frames > MAX_STALE_HANDSHAKE_FRAMES {
                return Err(Error::InvalidMux(
                    "too many stale frames before usbmux version response".to_owned(),
                ));
            }
            response.drain(..length);
        }

        let remaining = handshake_timeout
            .checked_sub(started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(Error::Timeout {
                operation: "reading usbmux version response",
                timeout: handshake_timeout,
            })?;
        let bytes = match transport.read(64 * 1024, remaining) {
            Ok(bytes) => bytes,
            Err(Error::Timeout { .. }) => {
                return Err(Error::Timeout {
                    operation: "reading usbmux version response",
                    timeout: handshake_timeout,
                });
            }
            Err(error) => return Err(error),
        };
        if response.len().saturating_add(bytes.len()) > MAX_MUX_PACKET {
            return Err(Error::InvalidMux(
                "version response exceeds framing limit".to_owned(),
            ));
        }
        response.extend_from_slice(&bytes);
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| Error::InvalidMux("truncated 16-bit field".to_owned()))?;
    Ok(u16::from_be_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| Error::InvalidMux("truncated 32-bit field".to_owned()))?;
    Ok(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[derive(Debug)]
    struct MockBulk {
        reads: Mutex<VecDeque<Result<Vec<u8>>>>,
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        write_timeouts: Arc<Mutex<Vec<Duration>>>,
    }

    impl BulkTransport for MockBulk {
        fn write(&self, bytes: &[u8], timeout: Duration) -> Result<()> {
            self.writes.lock().expect("writes").push(bytes.to_vec());
            self.write_timeouts
                .lock()
                .expect("write timeouts")
                .push(timeout);
            Ok(())
        }

        fn read(&self, _maximum: usize, timeout: Duration) -> Result<Vec<u8>> {
            self.reads
                .lock()
                .expect("reads")
                .pop_front()
                .unwrap_or(Err(Error::Timeout {
                    operation: "mock bulk read",
                    timeout,
                }))
        }
    }

    fn version_response() -> Vec<u8> {
        [
            PROTOCOL_VERSION.to_be_bytes(),
            20_u32.to_be_bytes(),
            2_u32.to_be_bytes(),
            0_u32.to_be_bytes(),
            0_u32.to_be_bytes(),
        ]
        .concat()
    }

    #[test]
    fn packet_codec_handles_fragmented_multiple_packets() {
        let first = MuxPacket {
            protocol: 2,
            transmit_sequence: 1,
            receive_sequence: 0,
            payload: vec![7],
        }
        .encode_with_magic(MUX_INCOMING_MAGIC)
        .expect("first");
        let second = MuxPacket {
            protocol: 6,
            transmit_sequence: 2,
            receive_sequence: 1,
            payload: vec![0; TCP_HEADER_LEN],
        }
        .encode_with_magic(MUX_INCOMING_MAGIC)
        .expect("second");
        let split = first.len() - 2;
        let mut codec = MuxPacketCodec::default();
        assert!(codec.push(&first[..split]).expect("fragment").is_empty());
        let packets = codec
            .push(&[&first[split..], &second].concat())
            .expect("packets");
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].payload, [7]);
    }

    #[test]
    fn connect_timeout_is_bounded() {
        let host = MuxHost::start(
            MockBulk {
                reads: Mutex::new(VecDeque::from([Ok(version_response())])),
                writes: Arc::new(Mutex::new(Vec::new())),
                write_timeouts: Arc::new(Mutex::new(Vec::new())),
            },
            Duration::from_millis(5),
            Duration::from_millis(1),
        )
        .expect("host");
        assert!(matches!(
            host.connect(62_078, Duration::from_millis(5)),
            Err(Error::Timeout {
                operation: "connecting usbmux TCP stream",
                ..
            })
        ));
    }

    #[test]
    fn version_handshake_accepts_fragmented_response() {
        let response = version_response();
        let host = MuxHost::start(
            MockBulk {
                reads: Mutex::new(VecDeque::from([
                    Ok(response[..5].to_vec()),
                    Ok(response[5..].to_vec()),
                ])),
                writes: Arc::new(Mutex::new(Vec::new())),
                write_timeouts: Arc::new(Mutex::new(Vec::new())),
            },
            Duration::from_millis(10),
            Duration::from_millis(1),
        )
        .expect("host");
        assert!(host.is_alive());
    }

    #[test]
    fn version_handshake_discards_captured_stale_tcp_syn_ack() {
        let stale_syn_ack = [
            0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x24, 0xfa, 0xce, 0xfa, 0xce, 0x00, 0x00,
            0x00, 0x01, 0xf2, 0x7e, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
            0x50, 0x12, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let response = version_response();
        let host = MuxHost::start(
            MockBulk {
                reads: Mutex::new(VecDeque::from([Ok(stale_syn_ack.to_vec()), Ok(response)])),
                writes: Arc::new(Mutex::new(Vec::new())),
                write_timeouts: Arc::new(Mutex::new(Vec::new())),
            },
            Duration::from_millis(10),
            Duration::from_millis(1),
        )
        .expect("host must resynchronize after the captured stale iPhone frame");
        assert!(host.is_alive());
    }

    #[test]
    fn version_handshake_rejects_stale_frame_with_bad_magic() {
        let mut stale = MuxPacket {
            protocol: PROTOCOL_TCP,
            transmit_sequence: 0,
            receive_sequence: 1,
            payload: vec![0; TCP_HEADER_LEN],
        }
        .encode_with_magic(MUX_INCOMING_MAGIC)
        .expect("stale packet");
        stale[8] ^= 0xff;
        let error = MuxHost::start(
            MockBulk {
                reads: Mutex::new(VecDeque::from([Ok(stale), Ok(version_response())])),
                writes: Arc::new(Mutex::new(Vec::new())),
                write_timeouts: Arc::new(Mutex::new(Vec::new())),
            },
            Duration::from_millis(10),
            Duration::from_millis(1),
        )
        .expect_err("malformed stale frame must fail closed");
        assert!(matches!(error, Error::InvalidMux(_)));
    }

    #[test]
    fn version_handshake_timeout_is_reported_at_protocol_boundary() {
        let error = MuxHost::start(
            MockBulk {
                reads: Mutex::new(VecDeque::new()),
                writes: Arc::new(Mutex::new(Vec::new())),
                write_timeouts: Arc::new(Mutex::new(Vec::new())),
            },
            Duration::from_millis(1),
            Duration::from_millis(1),
        )
        .expect_err("version handshake must time out");
        assert!(matches!(
            error,
            Error::Timeout {
                operation: "reading usbmux version response",
                ..
            }
        ));
    }

    #[test]
    fn connection_timeouts_are_validated_and_applied() {
        let write_timeouts = Arc::new(Mutex::new(Vec::new()));
        let host = MuxHost::start(
            MockBulk {
                reads: Mutex::new(VecDeque::from([Ok(version_response())])),
                writes: Arc::new(Mutex::new(Vec::new())),
                write_timeouts: Arc::clone(&write_timeouts),
            },
            Duration::from_millis(5),
            Duration::from_millis(1),
        )
        .expect("host");
        let (_sender, receiver) = mpsc::channel();
        let mut connection = MuxConnection {
            host,
            source_port: 1,
            destination_port: 2,
            local_sequence: 1,
            remote_sequence: 1,
            receiver,
            pending: VecDeque::new(),
            read_timeout: Duration::from_secs(1),
            write_timeout: Duration::from_secs(1),
            default_read_timeout: Duration::from_secs(1),
            default_write_timeout: Duration::from_secs(1),
            closed: false,
        };
        assert!(connection.set_read_timeout(Duration::ZERO).is_err());
        assert!(connection.set_write_timeout(Duration::ZERO).is_err());
        connection
            .set_read_timeout(Duration::from_millis(2))
            .expect("read timeout");
        connection
            .set_write_timeout(Duration::from_millis(17))
            .expect("write timeout");
        connection.write_all(b"payload").expect("write");
        assert_eq!(connection.read_timeout(), Duration::from_millis(2));
        assert_eq!(connection.write_timeout(), Duration::from_millis(17));
        assert_eq!(
            write_timeouts
                .lock()
                .expect("write timeouts")
                .last()
                .copied(),
            Some(Duration::from_millis(17))
        );
        let error = connection
            .read(&mut [0; 1])
            .expect_err("read must be bounded");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        connection.reset_read_timeout();
        connection.reset_write_timeout();
        assert_eq!(connection.read_timeout(), Duration::from_secs(1));
        assert_eq!(connection.write_timeout(), Duration::from_secs(1));
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut packet = MuxPacket {
            protocol: 2,
            transmit_sequence: 0,
            receive_sequence: 0,
            payload: vec![7],
        }
        .encode_with_magic(MUX_INCOMING_MAGIC)
        .expect("packet");
        packet[8] ^= 0xff;
        assert!(matches!(
            MuxPacketCodec::default().push(&packet),
            Err(Error::InvalidMux(_))
        ));
    }

    #[test]
    fn mux_magic_is_directional() {
        let packet = MuxPacket {
            protocol: PROTOCOL_SETUP,
            transmit_sequence: 0,
            receive_sequence: 0,
            payload: vec![7],
        };
        let outgoing = packet.encode().expect("outgoing packet");
        assert_eq!(&outgoing[8..12], &MUX_OUTGOING_MAGIC.to_be_bytes());
        assert!(matches!(
            MuxPacketCodec::default().push(&outgoing),
            Err(Error::InvalidMux(_))
        ));

        let incoming = packet
            .encode_with_magic(MUX_INCOMING_MAGIC)
            .expect("incoming packet");
        assert_eq!(&incoming[8..12], &MUX_INCOMING_MAGIC.to_be_bytes());
        assert_eq!(
            MuxPacketCodec::default()
                .push(&incoming)
                .expect("incoming magic")
                .as_slice(),
            &[packet]
        );
    }

    proptest! {
        #[test]
        fn packet_parser_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
            let _ = MuxPacketCodec::default().push(&bytes);
        }
    }
}
