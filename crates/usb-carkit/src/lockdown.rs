use std::{
    io::{Read, Write},
    thread,
    time::{Duration, Instant},
};

use plist::{Dictionary, Value};

use crate::{
    BulkTransport, CARKIT_SERVICE, CancellationToken, Error, FirstTimePairingPolicy, MuxConnection,
    MuxHost, PairRecord, PairRecordStore, Result, pairing::generate_pair_record, tls::TlsMuxStream,
};

pub const LOCKDOWN_PORT: u16 = 62_078;
const MAX_PLIST_BYTES: usize = 4 * 1024 * 1024;
const LABEL: &str = "preluded";

pub(crate) enum LockdownWire<B: BulkTransport> {
    Plain(MuxConnection<B>),
    Tls(Box<TlsMuxStream<B>>),
}

pub(crate) enum LockdownServiceWire<B: BulkTransport> {
    Plain(MuxConnection<B>),
    Tls(Box<TlsMuxStream<B>>),
}

impl<B: BulkTransport> Read for LockdownServiceWire<B> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(bytes),
            Self::Tls(stream) => stream.read(bytes),
        }
    }
}

impl<B: BulkTransport> LockdownServiceWire<B> {
    pub(crate) fn set_read_timeout(&mut self, timeout: Duration) -> Result<()> {
        match self {
            Self::Plain(stream) => stream.set_read_timeout(timeout),
            Self::Tls(stream) => stream.get_mut().set_read_timeout(timeout),
        }
    }
}

pub(crate) type CarkitLockdownServices<B> = (
    LockdownSession<B>,
    TlsMuxStream<B>,
    Option<LockdownServiceWire<B>>,
);

impl<B: BulkTransport> std::fmt::Debug for LockdownWire<B> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plain(stream) => formatter.debug_tuple("Plain").field(stream).finish(),
            Self::Tls(stream) => formatter
                .debug_tuple("Tls")
                .field(stream.get_ref())
                .finish(),
        }
    }
}

impl<B: BulkTransport> LockdownWire<B> {
    pub(crate) fn is_alive(&self) -> bool {
        match self {
            Self::Plain(stream) => stream.is_alive(),
            Self::Tls(stream) => stream.get_ref().is_alive(),
        }
    }
}

impl<B: BulkTransport> Read for LockdownWire<B> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(output),
            Self::Tls(stream) => stream.read(output),
        }
    }
}

impl<B: BulkTransport> Write for LockdownWire<B> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(bytes),
            Self::Tls(stream) => stream.write(bytes),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct LockdownSession<B: BulkTransport> {
    wire: LockdownWire<B>,
}

impl<B: BulkTransport> LockdownSession<B> {
    pub(crate) fn is_alive(&self) -> bool {
        self.wire.is_alive()
    }
}

pub(crate) fn start_carkit_service<B: BulkTransport>(
    host: &MuxHost<B>,
    device: &str,
    store: &PairRecordStore,
    timeout: Duration,
    first_time_pairing: FirstTimePairingPolicy,
    enable_syslog_tap: bool,
    cancellation: &CancellationToken,
) -> Result<CarkitLockdownServices<B>> {
    cancellation.check("starting Lockdown")?;
    let lockdown = host.connect(LOCKDOWN_PORT, timeout)?;
    let mut wire = LockdownWire::Plain(lockdown);

    let query = request("QueryType", Dictionary::new());
    let response = exchange(&mut wire, "QueryType", &query, timeout)?;
    cancellation.check("querying Lockdown")?;
    if response.get("Type").and_then(Value::as_string) != Some("com.apple.mobile.lockdown") {
        return Err(Error::InvalidLockdown(
            "connected service is not com.apple.mobile.lockdown".to_owned(),
        ));
    }

    let pairing_identifier = device;
    cancellation.check("loading Lockdown trust")?;

    let mut pairing = match store.load(pairing_identifier) {
        Ok(pairing) => pairing,
        Err(Error::PairingRecordMissing { .. }) if first_time_pairing.allow => pair_device(
            &mut wire,
            pairing_identifier,
            store,
            timeout,
            first_time_pairing,
            cancellation,
        )?,
        Err(error) => return Err(error),
    };

    let response = match start_session(&mut wire, &pairing, timeout) {
        Err(Error::LockdownRejected {
            request: "StartSession",
            reason,
        }) if reason == "InvalidHostID" && first_time_pairing.allow => {
            pairing = repair_pairing(
                &mut wire,
                pairing_identifier,
                store,
                pairing,
                timeout,
                first_time_pairing,
                cancellation,
            )?;
            cancellation.check("retrying the repaired Lockdown session")?;
            start_session(&mut wire, &pairing, timeout)?
        }
        result => result?,
    };
    cancellation.check("starting the trusted Lockdown session")?;
    response
        .get("SessionID")
        .and_then(Value::as_string)
        .ok_or_else(|| Error::InvalidLockdown("StartSession omitted SessionID".to_owned()))?;
    if response
        .get("EnableSessionSSL")
        .and_then(Value::as_boolean)
        .unwrap_or(false)
    {
        let plain = match wire {
            LockdownWire::Plain(stream) => stream,
            LockdownWire::Tls(_) => {
                return Err(Error::InvalidLockdown(
                    "Lockdown stream entered TLS twice".to_owned(),
                ));
            }
        };
        wire = LockdownWire::Tls(Box::new(crate::tls::connect_tls(plain, &pairing)?));
    }

    let service = start_service(&mut wire, host, &pairing, CARKIT_SERVICE, timeout)?;
    cancellation.check("starting the carkit service")?;
    let LockdownServiceWire::Tls(service) = service else {
        return Err(Error::NotReady {
            component: "carkit service",
            state: "tls_not_enabled",
        });
    };
    let syslog = enable_syslog_tap
        .then(|| start_service(&mut wire, host, &pairing, "com.apple.syslog_relay", timeout))
        .transpose()?;
    Ok((LockdownSession { wire }, *service, syslog))
}

fn start_session<S: Read + Write>(
    wire: &mut S,
    pairing: &PairRecord,
    timeout: Duration,
) -> Result<Dictionary> {
    let mut options = Dictionary::new();
    options.insert(
        "HostID".to_owned(),
        Value::String(pairing.host_id().to_owned()),
    );
    options.insert(
        "SystemBUID".to_owned(),
        Value::String(pairing.system_buid().to_owned()),
    );
    exchange(
        wire,
        "StartSession",
        &request("StartSession", options),
        timeout,
    )
}

fn start_service<B: BulkTransport>(
    wire: &mut LockdownWire<B>,
    host: &MuxHost<B>,
    pairing: &PairRecord,
    service: &str,
    timeout: Duration,
) -> Result<LockdownServiceWire<B>> {
    let mut options = Dictionary::new();
    options.insert("Service".to_owned(), Value::String(service.to_owned()));
    let response = exchange(
        wire,
        "StartService",
        &request("StartService", options),
        timeout,
    )?;
    let port = response
        .get("Port")
        .and_then(Value::as_unsigned_integer)
        .and_then(|port| u16::try_from(port).ok())
        .ok_or_else(|| {
            Error::InvalidLockdown("StartService returned an invalid Port".to_owned())
        })?;
    let stream = host.connect(port, timeout)?;
    if response
        .get("EnableServiceSSL")
        .and_then(Value::as_boolean)
        .unwrap_or(false)
    {
        Ok(LockdownServiceWire::Tls(Box::new(crate::tls::connect_tls(
            stream, pairing,
        )?)))
    } else {
        Ok(LockdownServiceWire::Plain(stream))
    }
}

fn pair_device<S: Read + Write>(
    wire: &mut S,
    device: &str,
    store: &PairRecordStore,
    io_timeout: Duration,
    policy: FirstTimePairingPolicy,
    cancellation: &CancellationToken,
) -> Result<PairRecord> {
    if policy.dialog_timeout.is_zero() || policy.retry_interval.is_zero() {
        return Err(Error::InvalidPairingRecord(
            "pairing dialog timeout and retry interval must be non-zero".to_owned(),
        ));
    }

    cancellation.check("preparing first-time pairing")?;
    let device_public_key = get_value_data(wire, "DevicePublicKey", io_timeout)?;
    let wifi_mac_address = match get_value(wire, "WiFiAddress", io_timeout) {
        Ok(value) => value.as_string().map(str::to_owned),
        Err(Error::LockdownRejected { reason, .. }) if reason == "MissingValue" => None,
        Err(error) => return Err(error),
    };
    let system_buid = store.system_buid()?;
    let pairing = generate_pair_record(&device_public_key, wifi_mac_address, system_buid)?;
    cancellation.check("generating first-time pairing credentials")?;

    let pair_request = pairing_request(&pairing);
    let mut exchange = WirePairingExchange {
        wire,
        request: &pair_request,
        io_timeout,
        started: Instant::now(),
        cancellation,
    };
    let response = wait_for_pairing_approval(&mut exchange, policy)?;
    persist_approved_pairing(device, store, pairing, &response)
}

fn repair_pairing<S: Read + Write>(
    wire: &mut S,
    device: &str,
    store: &PairRecordStore,
    pairing: PairRecord,
    io_timeout: Duration,
    policy: FirstTimePairingPolicy,
    cancellation: &CancellationToken,
) -> Result<PairRecord> {
    cancellation.check("repairing stale Lockdown pairing")?;
    let pair_request = pairing_request(&pairing);
    let mut exchange = WirePairingExchange {
        wire,
        request: &pair_request,
        io_timeout,
        started: Instant::now(),
        cancellation,
    };
    let response = wait_for_pairing_approval(&mut exchange, policy)?;
    persist_approved_pairing(device, store, pairing, &response)
}

fn pairing_request(pairing: &PairRecord) -> Dictionary {
    let mut pairing_options = Dictionary::new();
    pairing_options.insert("ExtendedPairingErrors".to_owned(), Value::Boolean(true));
    let mut options = Dictionary::new();
    options.insert("HostName".to_owned(), Value::String(local_hostname()));
    options.insert(
        "PairRecord".to_owned(),
        Value::Dictionary(pairing.pairing_dictionary()),
    );
    options.insert("ProtocolVersion".to_owned(), Value::String("2".to_owned()));
    options.insert(
        "PairingOptions".to_owned(),
        Value::Dictionary(pairing_options),
    );
    request("Pair", options)
}

fn persist_approved_pairing(
    device: &str,
    store: &PairRecordStore,
    mut pairing: PairRecord,
    response: &Dictionary,
) -> Result<PairRecord> {
    let escrow_bag = match response.get("EscrowBag") {
        None => None,
        Some(Value::Data(data)) if !data.is_empty() => Some(data.clone()),
        Some(_) => {
            return Err(Error::InvalidLockdown(
                "Pair response contains an invalid EscrowBag".to_owned(),
            ));
        }
    };
    pairing.set_escrow_bag(escrow_bag);
    store.save(device, &pairing)?;
    Ok(pairing)
}

trait PairingExchange {
    fn attempt(&mut self) -> Result<Dictionary>;
    fn elapsed(&self) -> Duration;
    fn sleep(&mut self, duration: Duration) -> Result<()>;
    fn is_cancelled(&self) -> bool {
        false
    }
}

struct WirePairingExchange<'a, S> {
    wire: &'a mut S,
    request: &'a Dictionary,
    io_timeout: Duration,
    started: Instant,
    cancellation: &'a CancellationToken,
}

impl<S: Read + Write> PairingExchange for WirePairingExchange<'_, S> {
    fn attempt(&mut self) -> Result<Dictionary> {
        exchange(self.wire, "Pair", self.request, self.io_timeout)
    }

    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    fn sleep(&mut self, duration: Duration) -> Result<()> {
        let deadline = Instant::now() + duration;
        loop {
            self.cancellation
                .check("waiting for iPhone trust approval")?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            thread::sleep(remaining.min(Duration::from_millis(50)));
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}

fn wait_for_pairing_approval(
    exchange: &mut impl PairingExchange,
    policy: FirstTimePairingPolicy,
) -> Result<Dictionary> {
    loop {
        if exchange.is_cancelled() {
            return Err(Error::Cancelled {
                operation: "waiting for iPhone trust approval",
            });
        }
        match exchange.attempt() {
            Ok(response) => return Ok(response),
            Err(Error::LockdownRejected { reason, .. })
                if reason == "PairingDialogResponsePending" || reason == "PasswordProtected" =>
            {
                let remaining = policy
                    .dialog_timeout
                    .checked_sub(exchange.elapsed())
                    .filter(|remaining| !remaining.is_zero())
                    .ok_or(Error::PairingDialogTimeout {
                        timeout: policy.dialog_timeout,
                    })?;
                exchange.sleep(policy.retry_interval.min(remaining))?;
            }
            Err(Error::LockdownRejected { reason, .. }) if reason == "UserDeniedPairing" => {
                return Err(Error::PairingDenied);
            }
            Err(error) => return Err(error),
        }
    }
}

fn get_value<S: Read + Write>(wire: &mut S, key: &'static str, timeout: Duration) -> Result<Value> {
    let mut options = Dictionary::new();
    options.insert("Key".to_owned(), Value::String(key.to_owned()));
    exchange(wire, "GetValue", &request("GetValue", options), timeout)?
        .remove("Value")
        .ok_or_else(|| Error::InvalidLockdown(format!("GetValue omitted {key}")))
}

fn get_value_data<S: Read + Write>(
    wire: &mut S,
    key: &'static str,
    timeout: Duration,
) -> Result<Vec<u8>> {
    get_value(wire, key, timeout)?
        .into_data()
        .filter(|data| !data.is_empty())
        .ok_or_else(|| Error::InvalidLockdown(format!("GetValue returned invalid {key}")))
}

fn local_hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|hostname| hostname.trim().to_owned())
        .filter(|hostname| !hostname.is_empty() && hostname.len() <= 255)
        .unwrap_or_else(|| "Prelude".to_owned())
}

fn request(name: &'static str, mut options: Dictionary) -> Dictionary {
    options.insert("Label".to_owned(), Value::String(LABEL.to_owned()));
    options.insert("Request".to_owned(), Value::String(name.to_owned()));
    options
}

fn exchange<S: Read + Write>(
    stream: &mut S,
    request_name: &'static str,
    request: &Dictionary,
    timeout: Duration,
) -> Result<Dictionary> {
    write_plist(stream, request)?;
    let response = read_plist(stream, timeout)?;
    validate_response(response, request_name)
}

fn validate_response(response: Value, request_name: &'static str) -> Result<Dictionary> {
    let response = response.into_dictionary().ok_or_else(|| {
        Error::InvalidLockdown(format!("{request_name} response is not a dictionary"))
    })?;
    if response.get("Request").and_then(Value::as_string) != Some(request_name) {
        return Err(Error::InvalidLockdown(format!(
            "{request_name} response has a mismatched Request"
        )));
    }
    if let Some(reason) = response.get("Error").and_then(Value::as_string) {
        return Err(Error::LockdownRejected {
            request: request_name,
            reason: reason.to_owned(),
        });
    }
    if response.get("Result").and_then(Value::as_string) == Some("Failure") {
        return Err(Error::LockdownRejected {
            request: request_name,
            reason: "Failure".to_owned(),
        });
    }
    Ok(response)
}

fn write_plist<S: Write>(stream: &mut S, dictionary: &Dictionary) -> Result<()> {
    let mut body = Vec::new();
    Value::Dictionary(dictionary.clone())
        .to_writer_xml(&mut body)
        .map_err(|error| Error::InvalidLockdown(error.to_string()))?;
    if body.len() > MAX_PLIST_BYTES {
        return Err(Error::InvalidLockdown(
            "outbound plist exceeds framing limit".to_owned(),
        ));
    }
    let length = u32::try_from(body.len())
        .map_err(|_| Error::InvalidLockdown("plist length exceeds u32".to_owned()))?;
    stream
        .write_all(&length.to_be_bytes())
        .map_err(|error| Error::io("writing Lockdown plist length", error))?;
    stream
        .write_all(&body)
        .map_err(|error| Error::io("writing Lockdown plist body", error))?;
    stream
        .flush()
        .map_err(|error| Error::io("flushing Lockdown plist", error))
}

fn read_plist<S: Read>(stream: &mut S, timeout: Duration) -> Result<Value> {
    let mut length = [0; 4];
    read_exact(
        stream,
        &mut length,
        "reading Lockdown plist length",
        timeout,
    )?;
    let length = usize::try_from(u32::from_be_bytes(length))
        .map_err(|_| Error::InvalidLockdown("plist length is not representable".to_owned()))?;
    if length == 0 || length > MAX_PLIST_BYTES {
        return Err(Error::InvalidLockdown(format!(
            "plist length {length} is outside 1..={MAX_PLIST_BYTES}"
        )));
    }
    let mut body = vec![0; length];
    read_exact(stream, &mut body, "reading Lockdown plist body", timeout)?;
    Value::from_reader(std::io::Cursor::new(body))
        .map_err(|error| Error::InvalidLockdown(error.to_string()))
}

fn read_exact<S: Read>(
    stream: &mut S,
    output: &mut [u8],
    operation: &'static str,
    timeout: Duration,
) -> Result<()> {
    stream.read_exact(output).map_err(|error| {
        if error.kind() == std::io::ErrorKind::TimedOut {
            Error::Timeout { operation, timeout }
        } else {
            Error::io(operation, error)
        }
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        io::{Cursor, ErrorKind},
    };

    use rand::{SeedableRng, rngs::StdRng};
    use rsa::{RsaPrivateKey, RsaPublicKey, pkcs1::EncodeRsaPublicKey};

    use super::*;

    #[test]
    fn oversized_plist_frame_is_rejected_before_allocation() {
        let mut bytes =
            Cursor::new((u32::try_from(MAX_PLIST_BYTES).expect("limit") + 1).to_be_bytes());
        assert!(matches!(
            read_plist(&mut bytes, Duration::from_secs(1)),
            Err(Error::InvalidLockdown(_))
        ));
    }

    #[derive(Debug)]
    struct TimeoutReader;

    impl Read for TimeoutReader {
        fn read(&mut self, _output: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(ErrorKind::TimedOut))
        }
    }

    #[test]
    fn plist_read_timeout_is_preserved() {
        assert!(matches!(
            read_plist(&mut TimeoutReader, Duration::from_millis(20)),
            Err(Error::Timeout {
                operation: "reading Lockdown plist length",
                ..
            })
        ));
    }

    #[test]
    fn path_like_pair_record_identifier_is_encoded_before_access() {
        let store = PairRecordStore::new("/var/lib/lockdown");
        assert!(matches!(
            store.load("../../etc/shadow"),
            Err(Error::PairingRecordMissing { .. })
        ));
    }

    #[test]
    fn missing_pair_record_never_generates_credentials() {
        let directory = tempfile::tempdir().expect("pairing store");
        let store = PairRecordStore::new(directory.path());
        assert!(matches!(
            store.load("device-udid"),
            Err(Error::PairingRecordMissing { .. })
        ));
        assert_eq!(
            fs::read_dir(directory.path()).expect("empty store").count(),
            0
        );
    }

    #[test]
    fn pairing_record_loads_required_fields_and_redacts_private_key() {
        let directory = tempfile::tempdir().expect("pairing store");
        let private_marker = b"private-key-must-not-appear".to_vec();
        let value = Value::Dictionary(Dictionary::from_iter([
            ("HostID".to_owned(), Value::String("host-id".to_owned())),
            (
                "SystemBUID".to_owned(),
                Value::String("system-buid".to_owned()),
            ),
            (
                "HostCertificate".to_owned(),
                Value::Data(b"host-certificate".to_vec()),
            ),
            (
                "HostPrivateKey".to_owned(),
                Value::Data(private_marker.clone()),
            ),
            (
                "DeviceCertificate".to_owned(),
                Value::Data(b"device-certificate".to_vec()),
            ),
            (
                "RootCertificate".to_owned(),
                Value::Data(b"root-certificate".to_vec()),
            ),
            (
                "RootPrivateKey".to_owned(),
                Value::Data(b"root-private-key".to_vec()),
            ),
        ]));
        let mut bytes = Vec::new();
        value.to_writer_xml(&mut bytes).expect("pair record plist");
        fs::write(directory.path().join("device-udid.plist"), bytes).expect("pair record");

        let record = PairRecordStore::new(directory.path())
            .load("device-udid")
            .expect("load pair record");
        assert_eq!(record.host_id(), "host-id");
        assert_eq!(record.host_private_key(), private_marker);
        assert!(!format!("{record:?}").contains("private-key-must-not-appear"));
    }

    #[test]
    fn malformed_response_request_is_rejected() {
        let response = Value::Dictionary(Dictionary::from_iter([(
            "Request".to_owned(),
            Value::String("Wrong".to_owned()),
        )]));
        let result = validate_response(response, "QueryType");
        assert!(matches!(result, Err(Error::InvalidLockdown(_))));
    }

    #[derive(Debug)]
    struct FakePairingExchange {
        attempts: VecDeque<Result<Dictionary>>,
        elapsed: Duration,
        sleeps: Vec<Duration>,
    }

    impl PairingExchange for FakePairingExchange {
        fn attempt(&mut self) -> Result<Dictionary> {
            self.attempts
                .pop_front()
                .unwrap_or_else(|| Err(Error::InvalidLockdown("unexpected attempt".to_owned())))
        }

        fn elapsed(&self) -> Duration {
            self.elapsed
        }

        fn sleep(&mut self, duration: Duration) -> Result<()> {
            self.elapsed += duration;
            self.sleeps.push(duration);
            Ok(())
        }
    }

    fn rejected(reason: &str) -> Result<Dictionary> {
        Err(Error::LockdownRejected {
            request: "Pair",
            reason: reason.to_owned(),
        })
    }

    fn pairing_policy() -> FirstTimePairingPolicy {
        FirstTimePairingPolicy {
            allow: true,
            dialog_timeout: Duration::from_secs(3),
            retry_interval: Duration::from_secs(1),
        }
    }

    #[test]
    fn pending_pairing_retries_until_user_approves() {
        let approved =
            Dictionary::from_iter([("Request".to_owned(), Value::String("Pair".to_owned()))]);
        let mut exchange = FakePairingExchange {
            attempts: VecDeque::from([
                rejected("PairingDialogResponsePending"),
                rejected("PairingDialogResponsePending"),
                Ok(approved.clone()),
            ]),
            elapsed: Duration::ZERO,
            sleeps: Vec::new(),
        };
        assert_eq!(
            wait_for_pairing_approval(&mut exchange, pairing_policy()).expect("approved"),
            approved
        );
        assert_eq!(exchange.sleeps, [Duration::from_secs(1); 2]);
    }

    #[test]
    fn locked_phone_retries_on_one_pairing_connection_until_unlocked() {
        let approved =
            Dictionary::from_iter([("Request".to_owned(), Value::String("Pair".to_owned()))]);
        let mut exchange = FakePairingExchange {
            attempts: VecDeque::from([
                rejected("PasswordProtected"),
                rejected("PasswordProtected"),
                Ok(approved.clone()),
            ]),
            elapsed: Duration::ZERO,
            sleeps: Vec::new(),
        };
        assert_eq!(
            wait_for_pairing_approval(&mut exchange, pairing_policy()).expect("unlocked"),
            approved
        );
        assert_eq!(exchange.sleeps, [Duration::from_secs(1); 2]);
    }

    #[test]
    fn pending_pairing_has_a_hard_deadline() {
        let mut exchange = FakePairingExchange {
            attempts: VecDeque::from([
                rejected("PairingDialogResponsePending"),
                rejected("PairingDialogResponsePending"),
                rejected("PairingDialogResponsePending"),
            ]),
            elapsed: Duration::ZERO,
            sleeps: Vec::new(),
        };
        let policy = FirstTimePairingPolicy {
            allow: true,
            dialog_timeout: Duration::from_secs(2),
            retry_interval: Duration::from_secs(1),
        };
        assert!(matches!(
            wait_for_pairing_approval(&mut exchange, policy),
            Err(Error::PairingDialogTimeout { .. })
        ));
    }

    #[test]
    fn user_denial_is_terminal_and_never_retried() {
        let mut exchange = FakePairingExchange {
            attempts: VecDeque::from([rejected("UserDeniedPairing")]),
            elapsed: Duration::ZERO,
            sleeps: Vec::new(),
        };
        assert!(matches!(
            wait_for_pairing_approval(&mut exchange, pairing_policy()),
            Err(Error::PairingDenied)
        ));
        assert!(exchange.sleeps.is_empty());
    }

    #[test]
    fn cancelled_pairing_wait_is_terminal_before_an_attempt() {
        #[derive(Debug)]
        struct CancelledExchange;

        impl PairingExchange for CancelledExchange {
            fn attempt(&mut self) -> Result<Dictionary> {
                panic!("cancelled exchange must not attempt pairing")
            }

            fn elapsed(&self) -> Duration {
                Duration::ZERO
            }

            fn sleep(&mut self, _duration: Duration) -> Result<()> {
                Ok(())
            }

            fn is_cancelled(&self) -> bool {
                true
            }
        }

        assert!(matches!(
            wait_for_pairing_approval(&mut CancelledExchange, pairing_policy()),
            Err(Error::Cancelled {
                operation: "waiting for iPhone trust approval"
            })
        ));
    }

    #[test]
    fn in_flight_pending_pairing_observes_cancellation_during_wait() {
        #[derive(Debug, Default)]
        struct CancellingExchange {
            attempts: usize,
            cancelled: bool,
        }

        impl PairingExchange for CancellingExchange {
            fn attempt(&mut self) -> Result<Dictionary> {
                self.attempts += 1;
                rejected("PairingDialogResponsePending")
            }

            fn elapsed(&self) -> Duration {
                Duration::ZERO
            }

            fn sleep(&mut self, _duration: Duration) -> Result<()> {
                self.cancelled = true;
                Ok(())
            }

            fn is_cancelled(&self) -> bool {
                self.cancelled
            }
        }

        let mut exchange = CancellingExchange::default();
        assert!(matches!(
            wait_for_pairing_approval(&mut exchange, pairing_policy()),
            Err(Error::Cancelled {
                operation: "waiting for iPhone trust approval"
            })
        ));
        assert_eq!(exchange.attempts, 1);
    }

    #[test]
    fn malformed_pairing_failure_is_not_accepted() {
        let response = Value::Dictionary(Dictionary::from_iter([
            ("Request".to_owned(), Value::String("Pair".to_owned())),
            ("Result".to_owned(), Value::String("Failure".to_owned())),
        ]));
        assert!(matches!(
            validate_response(response, "Pair"),
            Err(Error::LockdownRejected { reason, .. }) if reason == "Failure"
        ));
    }

    #[test]
    fn approved_pairing_persists_the_returned_escrow_bag() {
        let pair_record = Value::Dictionary(Dictionary::from_iter([
            ("HostID".to_owned(), Value::String("host-id".to_owned())),
            (
                "SystemBUID".to_owned(),
                Value::String("system-buid".to_owned()),
            ),
            (
                "HostCertificate".to_owned(),
                Value::Data(b"host-certificate".to_vec()),
            ),
            (
                "HostPrivateKey".to_owned(),
                Value::Data(b"host-private-key".to_vec()),
            ),
            (
                "DeviceCertificate".to_owned(),
                Value::Data(b"device-certificate".to_vec()),
            ),
            (
                "RootCertificate".to_owned(),
                Value::Data(b"root-certificate".to_vec()),
            ),
            (
                "RootPrivateKey".to_owned(),
                Value::Data(b"root-private-key".to_vec()),
            ),
        ]));
        let mut bytes = Vec::new();
        pair_record
            .to_writer_xml(&mut bytes)
            .expect("pair record plist");
        let pair_record = PairRecord::parse(&bytes).expect("pair record");
        let temporary = tempfile::tempdir().expect("temporary directory");
        let store = PairRecordStore::new(temporary.path().join("lockdown"));
        let response = Dictionary::from_iter([
            ("Request".to_owned(), Value::String("Pair".to_owned())),
            ("EscrowBag".to_owned(), Value::Data(vec![7, 8, 9])),
        ]);
        persist_approved_pairing("device-id", &store, pair_record, &response)
            .expect("persist approved pairing");

        let persisted =
            Value::from_file(store.directory().join("device-id.plist")).expect("persisted plist");
        assert_eq!(
            persisted
                .as_dictionary()
                .and_then(|dictionary| dictionary.get("EscrowBag"))
                .and_then(Value::as_data),
            Some([7, 8, 9].as_slice())
        );
    }

    #[test]
    fn stale_host_repair_reuses_public_pairing_identity_without_private_keys() {
        let pair_record = Value::Dictionary(Dictionary::from_iter([
            ("HostID".to_owned(), Value::String("host-id".to_owned())),
            (
                "SystemBUID".to_owned(),
                Value::String("system-buid".to_owned()),
            ),
            (
                "HostCertificate".to_owned(),
                Value::Data(b"host-certificate".to_vec()),
            ),
            (
                "HostPrivateKey".to_owned(),
                Value::Data(b"host-private-key".to_vec()),
            ),
            (
                "DeviceCertificate".to_owned(),
                Value::Data(b"device-certificate".to_vec()),
            ),
            (
                "RootCertificate".to_owned(),
                Value::Data(b"root-certificate".to_vec()),
            ),
            (
                "RootPrivateKey".to_owned(),
                Value::Data(b"root-private-key".to_vec()),
            ),
        ]));
        let mut bytes = Vec::new();
        pair_record
            .to_writer_xml(&mut bytes)
            .expect("pair record plist");
        let pair_record = PairRecord::parse(&bytes).expect("pair record");
        let response = framed_response([
            ("Request", Value::String("Pair".to_owned())),
            ("EscrowBag", Value::Data(vec![1, 2, 3])),
        ]);
        let mut wire = ScriptedWire {
            reads: Cursor::new(response),
            writes: Vec::new(),
        };
        let temporary = tempfile::tempdir().expect("temporary directory");
        let store = PairRecordStore::new(temporary.path().join("lockdown"));
        repair_pairing(
            &mut wire,
            "device-id",
            &store,
            pair_record,
            Duration::from_millis(10),
            pairing_policy(),
            &CancellationToken::default(),
        )
        .expect("repair pairing");

        let requests = written_requests(&wire.writes);
        assert_eq!(requests.len(), 1);
        let sent_record = requests[0]
            .as_dictionary()
            .and_then(|request| request.get("PairRecord"))
            .and_then(Value::as_dictionary)
            .expect("sent pair record");
        assert_eq!(
            sent_record.get("HostID").and_then(Value::as_string),
            Some("host-id")
        );
        assert!(!sent_record.contains_key("HostPrivateKey"));
        assert!(!sent_record.contains_key("RootPrivateKey"));
        let persisted = Value::from_file(store.directory().join("device-id.plist"))
            .expect("persisted repaired record");
        assert_eq!(
            persisted
                .as_dictionary()
                .and_then(|dictionary| dictionary.get("EscrowBag"))
                .and_then(Value::as_data),
            Some([1, 2, 3].as_slice())
        );
    }

    struct ScriptedWire {
        reads: Cursor<Vec<u8>>,
        writes: Vec<u8>,
    }

    impl Read for ScriptedWire {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            self.reads.read(output)
        }
    }

    impl Write for ScriptedWire {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.writes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn framed_response(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Vec<u8> {
        let mut body = Vec::new();
        Value::Dictionary(Dictionary::from_iter(entries))
            .to_writer_xml(&mut body)
            .expect("response plist");
        let mut framed = u32::try_from(body.len())
            .expect("response length")
            .to_be_bytes()
            .to_vec();
        framed.extend(body);
        framed
    }

    fn written_requests(mut bytes: &[u8]) -> Vec<Value> {
        let mut requests = Vec::new();
        while !bytes.is_empty() {
            let length = u32::from_be_bytes(bytes[..4].try_into().expect("request length"));
            let length = usize::try_from(length).expect("request length usize");
            requests.push(
                Value::from_reader(Cursor::new(&bytes[4..4 + length])).expect("request plist"),
            );
            bytes = &bytes[4 + length..];
        }
        requests
    }

    #[test]
    fn pending_then_denied_pairing_never_persists_keys() {
        let mut rng = StdRng::seed_from_u64(17);
        let device_key = RsaPrivateKey::new(&mut rng, 2048).expect("device key");
        let device_public_key = RsaPublicKey::from(&device_key)
            .to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
            .expect("device public key")
            .into_bytes();
        let responses = [
            framed_response([
                ("Request", Value::String("GetValue".to_owned())),
                ("Value", Value::Data(device_public_key)),
            ]),
            framed_response([
                ("Request", Value::String("GetValue".to_owned())),
                ("Error", Value::String("MissingValue".to_owned())),
            ]),
            framed_response([
                ("Request", Value::String("Pair".to_owned())),
                (
                    "Error",
                    Value::String("PairingDialogResponsePending".to_owned()),
                ),
            ]),
            framed_response([
                ("Request", Value::String("Pair".to_owned())),
                ("Error", Value::String("UserDeniedPairing".to_owned())),
            ]),
        ]
        .concat();
        let mut wire = ScriptedWire {
            reads: Cursor::new(responses),
            writes: Vec::new(),
        };
        let temporary = tempfile::tempdir().expect("temporary directory");
        let directory = temporary.path().join("lockdown");
        let store = PairRecordStore::new(&directory);
        let error = pair_device(
            &mut wire,
            "device-id",
            &store,
            Duration::from_millis(10),
            FirstTimePairingPolicy {
                allow: true,
                dialog_timeout: Duration::from_millis(20),
                retry_interval: Duration::from_millis(1),
            },
            &CancellationToken::default(),
        )
        .expect_err("user denial");
        assert!(matches!(error, Error::PairingDenied));
        assert!(!directory.exists());

        let pair_requests = written_requests(&wire.writes)
            .into_iter()
            .filter_map(Value::into_dictionary)
            .filter(|request| request.get("Request").and_then(Value::as_string) == Some("Pair"))
            .collect::<Vec<_>>();
        assert_eq!(pair_requests.len(), 2);
        assert_eq!(pair_requests[0], pair_requests[1]);
        assert_eq!(
            pair_requests[0]
                .get("ProtocolVersion")
                .and_then(Value::as_string),
            Some("2")
        );
        assert_eq!(
            pair_requests[0]
                .get("PairingOptions")
                .and_then(Value::as_dictionary)
                .and_then(|options| options.get("ExtendedPairingErrors"))
                .and_then(Value::as_boolean),
            Some(true)
        );
        let pair_record = pair_requests[0]
            .get("PairRecord")
            .and_then(Value::as_dictionary)
            .expect("pair record");
        assert!(!pair_record.contains_key("RootPrivateKey"));
        assert!(!pair_record.contains_key("HostPrivateKey"));
    }
}
