use std::{
    collections::BTreeMap,
    fmt,
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use plist::{Dictionary, Value};
use prelude_iap2::CarkitIo;
use prelude_net::{ControlCipher, ControlKeys, StreamKey};
use prelude_usb_carkit::CancellationToken;

use crate::airplay_runtime::EventWorkerCommand;

const IAP2_TUNNEL_CLIENT_TYPE: &str = "E9459FD0-BCAD-4C45-820F-1E72447EF2F2";
const HEADER_LENGTH: usize = 32;
const MAXIMUM_MESSAGE_LENGTH: usize = 1024 * 1024;
const ACCEPT_POLL: Duration = Duration::from_millis(10);
const READ_POLL: Duration = Duration::from_millis(100);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const OUTBOUND_POLL: Duration = Duration::from_millis(5);
const TUNNEL_BACKLOG: usize = 64;
const SYNC: &[u8] = b"sync";
const REPLY: &[u8] = b"rply";
const IAP2_COMMAND: [u8; 4] = *b"comm";

pub(crate) type SharedIap2Tunnel = Arc<Mutex<Option<Iap2Tunnel>>>;

pub(crate) struct Iap2Tunnel {
    inbound: Receiver<Vec<u8>>,
    outbound: SyncSender<EventWorkerCommand>,
    pending: Vec<u8>,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
}

impl fmt::Debug for Iap2Tunnel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Iap2Tunnel")
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl Iap2Tunnel {
    fn new(inbound: Receiver<Vec<u8>>, outbound: SyncSender<EventWorkerCommand>) -> Self {
        Self {
            inbound,
            outbound,
            pending: Vec::new(),
            read_timeout: None,
            write_timeout: None,
        }
    }
}

impl CarkitIo for Iap2Tunnel {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.read_timeout = timeout;
        Ok(())
    }

    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.write_timeout = timeout;
        Ok(())
    }
}

impl Read for Iap2Tunnel {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.pending.is_empty() {
            let received = match self.read_timeout {
                Some(timeout) => self.inbound.recv_timeout(timeout),
                None => self
                    .inbound
                    .recv()
                    .map_err(|_| RecvTimeoutError::Disconnected),
            };
            match received {
                Ok(bytes) => self.pending = bytes,
                Err(RecvTimeoutError::Timeout) => return Err(io::ErrorKind::TimedOut.into()),
                Err(RecvTimeoutError::Disconnected) => return Ok(0),
            }
        }
        let length = buffer.len().min(self.pending.len());
        buffer[..length].copy_from_slice(&self.pending[..length]);
        self.pending.drain(..length);
        Ok(length)
    }
}

impl Write for Iap2Tunnel {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let deadline = self.write_timeout.map(|timeout| Instant::now() + timeout);
        let mut command = EventWorkerCommand::IapSendMessage(buffer.to_vec());
        loop {
            match self.outbound.try_send(command) {
                Ok(()) => return Ok(buffer.len()),
                Err(TrySendError::Disconnected(_)) => return Err(io::ErrorKind::BrokenPipe.into()),
                Err(TrySendError::Full(returned)) => {
                    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        return Err(io::ErrorKind::TimedOut.into());
                    }
                    command = returned;
                    thread::sleep(OUTBOUND_POLL);
                }
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct DataChannels {
    event_commands: SyncSender<EventWorkerCommand>,
    iap2_tunnel: SharedIap2Tunnel,
    channels: BTreeMap<StreamKey, (CancellationToken, JoinHandle<()>)>,
}

impl DataChannels {
    pub(crate) fn new(
        event_commands: SyncSender<EventWorkerCommand>,
        iap2_tunnel: SharedIap2Tunnel,
    ) -> Self {
        Self {
            event_commands,
            iap2_tunnel,
            channels: BTreeMap::new(),
        }
    }

    pub(crate) fn open(
        &mut self,
        key: StreamKey,
        listener: TcpListener,
        keys: ControlKeys,
        client_type: Option<String>,
    ) -> Result<(), String> {
        let tunnel = client_type
            .as_deref()
            .is_some_and(|client_type| client_type.eq_ignore_ascii_case(IAP2_TUNNEL_CLIENT_TYPE))
            .then(|| (self.event_commands.clone(), Arc::clone(&self.iap2_tunnel)));
        let cancellation = CancellationToken::default();
        let worker_cancellation = cancellation.clone();
        let worker = thread::Builder::new()
            .name("prelude-airplay-data".to_owned())
            .spawn(
                move || match serve(&listener, keys, tunnel, &worker_cancellation) {
                    Ok(()) => tracing::info!(?client_type, "AirPlay data channel closed"),
                    Err(error) => {
                        tracing::warn!(?client_type, %error, "AirPlay data channel failed");
                    }
                },
            )
            .map_err(|error| format!("starting the AirPlay data channel failed: {error}"))?;
        if let Some(previous) = self.channels.insert(key, (cancellation, worker)) {
            stop(previous);
        }
        Ok(())
    }

    pub(crate) fn close(&mut self, keys: &[StreamKey]) {
        for key in keys {
            if let Some(channel) = self.channels.remove(key) {
                stop(channel);
            }
        }
    }

    pub(crate) fn close_all(&mut self) {
        for (_, channel) in std::mem::take(&mut self.channels) {
            stop(channel);
        }
    }
}

impl Drop for DataChannels {
    fn drop(&mut self) {
        self.close_all();
    }
}

fn stop((cancellation, worker): (CancellationToken, JoinHandle<()>)) {
    cancellation.cancel();
    let _ = worker.join();
}

fn serve(
    listener: &TcpListener,
    keys: ControlKeys,
    tunnel: Option<(SyncSender<EventWorkerCommand>, SharedIap2Tunnel)>,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    let Some(mut stream) = accept(listener, cancellation)? else {
        return Ok(());
    };
    let mut cipher = ControlCipher::new(keys)
        .map_err(|error| format!("installing data channel keys failed: {error}"))?;
    let iap2 = match tunnel {
        Some((outbound, slot)) => {
            let (sender, receiver) = mpsc::sync_channel(TUNNEL_BACKLOG);
            *slot
                .lock()
                .map_err(|_| "the iAP2 tunnel slot is poisoned".to_owned())? =
                Some(Iap2Tunnel::new(receiver, outbound));
            Some(sender)
        }
        None => None,
    };
    let mut input = [0_u8; 16 * 1024];
    let mut plaintext = Vec::new();
    while !cancellation.is_cancelled() {
        let length = match stream.read(&mut input) {
            Ok(0) => return Ok(()),
            Ok(length) => length,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => return Err(format!("reading the data channel failed: {error}")),
        };
        plaintext.extend(
            cipher
                .decrypt(&input[..length])
                .map_err(|error| format!("decrypting the data channel failed: {error}"))?,
        );
        while let Some(message) = next_message(&mut plaintext)? {
            tracing::debug!(
                kind = %String::from_utf8_lossy(&message.kind),
                command = %String::from_utf8_lossy(&message.command),
                sequence = format_args!("{:#018x}", message.sequence),
                payload_length = message.payload.len(),
                "AirPlay data channel message"
            );
            match &iap2 {
                Some(iap2) => {
                    if message.command == IAP2_COMMAND && iap2.send(message.payload).is_err() {
                        return Ok(());
                    }
                }
                None => {
                    tracing::debug!(
                        payload_keys = %payload_keys(&message.payload),
                        "AirPlay data channel payload"
                    );
                    if message.kind == SYNC {
                        let reply = cipher.encrypt(&reply(&message)?).map_err(|error| {
                            format!("encrypting a data channel reply failed: {error}")
                        })?;
                        stream.write_all(&reply).map_err(|error| {
                            format!("writing a data channel reply failed: {error}")
                        })?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn accept(
    listener: &TcpListener,
    cancellation: &CancellationToken,
) -> Result<Option<TcpStream>, String> {
    let local = listener
        .local_addr()
        .map_err(|error| format!("reading the data channel address failed: {error}"))?;
    loop {
        if cancellation.is_cancelled() {
            return Ok(None);
        }
        match listener.accept() {
            Ok((stream, peer)) => {
                if !same_link(local, peer) {
                    return Err(format!(
                        "data channel peer {peer} is not on the receiver's link"
                    ));
                }
                tracing::info!(%peer, "AirPlay data channel connected");
                stream
                    .set_nonblocking(false)
                    .and_then(|()| stream.set_read_timeout(Some(READ_POLL)))
                    .and_then(|()| stream.set_write_timeout(Some(WRITE_TIMEOUT)))
                    .map_err(|error| format!("configuring the data channel failed: {error}"))?;
                return Ok(Some(stream));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL);
            }
            Err(error) => return Err(format!("accepting the data channel failed: {error}")),
        }
    }
}

fn same_link(local: SocketAddr, peer: SocketAddr) -> bool {
    matches!(
        (local, peer),
        (SocketAddr::V6(local), SocketAddr::V6(peer))
            if peer.ip().is_unicast_link_local() && peer.scope_id() == local.scope_id()
    )
}

#[derive(Debug, PartialEq, Eq)]
struct Message {
    kind: Vec<u8>,
    command: [u8; 4],
    sequence: u64,
    payload: Vec<u8>,
}

fn next_message(buffer: &mut Vec<u8>) -> Result<Option<Message>, String> {
    let Some(size) = buffer.first_chunk::<4>() else {
        return Ok(None);
    };
    let size = u32::from_be_bytes(*size) as usize;
    if !(HEADER_LENGTH..=MAXIMUM_MESSAGE_LENGTH).contains(&size) {
        return Err(format!("data channel message size {size} is out of range"));
    }
    if buffer.len() < size {
        return Ok(None);
    }
    let message: Vec<u8> = buffer.drain(..size).collect();
    let kind = message[4..16]
        .split(|byte| *byte == 0)
        .next()
        .unwrap_or_default()
        .to_vec();
    Ok(Some(Message {
        kind,
        command: message[16..20].try_into().expect("four-byte command"),
        sequence: u64::from_be_bytes(message[20..28].try_into().expect("eight-byte sequence")),
        payload: message[HEADER_LENGTH..].to_vec(),
    }))
}

fn reply(request: &Message) -> Result<Vec<u8>, String> {
    let mut payload = Vec::new();
    if !request.payload.is_empty() {
        Value::Dictionary(Dictionary::new())
            .to_writer_binary(&mut payload)
            .map_err(|error| format!("encoding a data channel reply failed: {error}"))?;
    }
    let size = u32::try_from(HEADER_LENGTH + payload.len())
        .map_err(|_| "data channel reply is too large".to_owned())?;
    let mut message = Vec::with_capacity(HEADER_LENGTH + payload.len());
    message.extend_from_slice(&size.to_be_bytes());
    message.extend_from_slice(REPLY);
    message.resize(16, 0);
    message.extend_from_slice(&[0; 4]);
    message.extend_from_slice(&request.sequence.to_be_bytes());
    message.extend_from_slice(&[0; 4]);
    message.extend_from_slice(&payload);
    Ok(message)
}

fn payload_keys(payload: &[u8]) -> String {
    if payload.is_empty() {
        return String::new();
    }
    match Value::from_reader(io::Cursor::new(payload)) {
        Ok(Value::Dictionary(dictionary)) => describe(&dictionary),
        Ok(_) => "non-dictionary".to_owned(),
        Err(_) => "undecodable".to_owned(),
    }
}

fn describe(dictionary: &Dictionary) -> String {
    let fields: Vec<String> = dictionary
        .iter()
        .map(|(key, value)| match value {
            Value::Dictionary(nested) => format!("{key}{{{}}}", describe(nested)),
            _ => key.clone(),
        })
        .collect();
    fields.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(kind: &[u8], command: &[u8; 4], sequence: u64, payload: &[u8]) -> Vec<u8> {
        let mut message = Vec::new();
        message.extend_from_slice(&((HEADER_LENGTH + payload.len()) as u32).to_be_bytes());
        message.extend_from_slice(kind);
        message.resize(16, 0);
        message.extend_from_slice(command);
        message.extend_from_slice(&sequence.to_be_bytes());
        message.extend_from_slice(&[0; 4]);
        message.extend_from_slice(payload);
        message
    }

    #[test]
    fn sync_without_payload_gets_a_header_only_reply() {
        let mut buffer =
            hex::decode("0000002073796e630000000000000000636d6e64cf4934469b4941ae00000000")
                .expect("hex");
        let message = next_message(&mut buffer).expect("parse").expect("message");
        assert!(buffer.is_empty());
        assert_eq!(message.kind, SYNC);
        assert_eq!(&message.command, b"cmnd");
        assert_eq!(message.sequence, 0xcf49_3446_9b49_41ae);

        assert_eq!(
            hex::encode(reply(&message).expect("reply")),
            "0000002072706c79000000000000000000000000cf4934469b4941ae00000000"
        );
    }

    #[test]
    fn sync_with_payload_gets_an_empty_dictionary() {
        let mut params = Dictionary::new();
        params.insert("data".to_owned(), Value::Data(vec![1, 2, 3]));
        let mut body = Dictionary::new();
        body.insert("params".to_owned(), Value::Dictionary(params));
        let mut payload = Vec::new();
        Value::Dictionary(body)
            .to_writer_binary(&mut payload)
            .expect("plist");
        let mut buffer = header(SYNC, b"comm", 0x1_6155_c3e0, &payload);
        let message = next_message(&mut buffer).expect("parse").expect("message");
        assert_eq!(payload_keys(&message.payload), "params{data}");

        let reply = reply(&message).expect("reply");
        let mut parsed = reply.clone();
        let parsed = next_message(&mut parsed).expect("parse").expect("reply");
        assert_eq!(parsed.kind, REPLY);
        assert_eq!(parsed.command, [0; 4]);
        assert_eq!(parsed.sequence, 0x1_6155_c3e0);
        assert_eq!(
            Value::from_reader(io::Cursor::new(&parsed.payload)).expect("plist"),
            Value::Dictionary(Dictionary::new())
        );
    }

    #[test]
    fn tunnel_reads_phone_bytes_and_sends_accessory_bytes_as_event_commands() {
        let (phone, inbound) = mpsc::sync_channel(4);
        let (outbound, commands) = mpsc::sync_channel(4);
        let mut tunnel = Iap2Tunnel::new(inbound, outbound);
        tunnel
            .set_read_timeout(Some(Duration::from_millis(10)))
            .expect("read timeout");

        phone.send(vec![1, 2, 3]).expect("phone bytes");
        let mut buffer = [0; 2];
        assert_eq!(tunnel.read(&mut buffer).expect("first read"), 2);
        assert_eq!(buffer, [1, 2]);
        assert_eq!(tunnel.read(&mut buffer).expect("second read"), 1);
        assert_eq!(buffer[0], 3);
        assert_eq!(
            tunnel.read(&mut buffer).expect_err("idle").kind(),
            io::ErrorKind::TimedOut
        );

        assert_eq!(tunnel.write(&[0xff, 0x5a]).expect("write"), 2);
        let EventWorkerCommand::IapSendMessage(sent) = commands.try_recv().expect("command") else {
            panic!("tunnel writes must become iAPSendMessage commands");
        };
        assert_eq!(sent, [0xff, 0x5a]);

        drop(phone);
        assert_eq!(tunnel.read(&mut buffer).expect("end of stream"), 0);
    }

    #[test]
    fn tunnel_writes_time_out_while_the_event_channel_is_backed_up() {
        let (_phone, inbound) = mpsc::sync_channel(1);
        let (outbound, _commands) = mpsc::sync_channel(1);
        let mut tunnel = Iap2Tunnel::new(inbound, outbound);
        tunnel
            .set_write_timeout(Some(Duration::from_millis(20)))
            .expect("write timeout");
        tunnel.write_all(&[1]).expect("first command fits");
        assert_eq!(
            tunnel.write(&[2]).expect_err("full").kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn partial_messages_wait_for_the_rest() {
        let message = header(SYNC, b"comm", 7, &[0xaa; 8]);
        let mut buffer = message[..20].to_vec();
        assert_eq!(next_message(&mut buffer).expect("partial"), None);
        buffer.extend_from_slice(&message[20..]);
        assert!(next_message(&mut buffer).expect("complete").is_some());
    }

    #[test]
    fn out_of_range_sizes_are_rejected() {
        let mut undersized = vec![0, 0, 0, 8, 0, 0, 0, 0];
        assert!(next_message(&mut undersized).is_err());
        let mut oversized = ((MAXIMUM_MESSAGE_LENGTH + 1) as u32).to_be_bytes().to_vec();
        assert!(next_message(&mut oversized).is_err());
    }
}
