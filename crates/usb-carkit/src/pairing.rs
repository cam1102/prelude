use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

use der::{EncodePem, pem::LineEnding, referenced::OwnedToRef};
use plist::{Dictionary, Value};
use rand::{CryptoRng, RngCore, rngs::OsRng};
use rsa::{
    RsaPrivateKey, RsaPublicKey,
    pkcs1::DecodeRsaPublicKey,
    pkcs1v15::{Signature, SigningKey},
    pkcs8::{DecodePublicKey, EncodePrivateKey, EncodePublicKey},
    traits::PublicKeyParts,
};
use sha2::Sha256;
use uuid::Uuid;
use x509_cert::{
    Certificate,
    builder::{Builder, CertificateBuilder, Profile},
    ext::pkix::{BasicConstraints, KeyUsage, KeyUsages, SubjectKeyIdentifier},
    name::Name,
    serial_number::SerialNumber,
    spki::SubjectPublicKeyInfoOwned,
    time::{Time, Validity},
};
use zeroize::Zeroizing;

use crate::{Error, Result};

const MAX_PAIR_RECORD_BYTES: u64 = 1024 * 1024;
const MAX_DEVICE_IDENTIFIER_BYTES: usize = 96;
const SYSTEM_CONFIGURATION_FILE: &str = "SystemConfiguration.plist";
const SYSTEM_BUID_KEY: &str = "SystemBUID";
const CERTIFICATE_VALIDITY: Duration = Duration::from_secs(10 * 365 * 24 * 60 * 60);
const CLOCK_SKEW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub(crate) fn check(&self, operation: &'static str) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::Cancelled { operation })
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstTimePairingPolicy {
    pub allow: bool,
    pub dialog_timeout: Duration,
    pub retry_interval: Duration,
}

impl Default for FirstTimePairingPolicy {
    fn default() -> Self {
        Self {
            allow: false,
            dialog_timeout: Duration::from_secs(60),
            retry_interval: Duration::from_secs(1),
        }
    }
}

pub struct PairRecord {
    host_id: String,
    system_buid: String,
    host_certificate: Vec<u8>,
    host_private_key: Zeroizing<Vec<u8>>,
    device_certificate: Vec<u8>,
    root_certificate: Vec<u8>,
    root_private_key: Zeroizing<Vec<u8>>,
    wifi_mac_address: Option<String>,
    escrow_bag: Option<Zeroizing<Vec<u8>>>,
}

impl std::fmt::Debug for PairRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PairRecord")
            .field("host_id", &"[redacted]")
            .field("system_buid", &"[redacted]")
            .field(
                "host_certificate",
                &format_args!("{} bytes", self.host_certificate.len()),
            )
            .field("host_private_key", &"[redacted]")
            .field(
                "device_certificate",
                &format_args!("{} bytes", self.device_certificate.len()),
            )
            .field(
                "root_certificate",
                &format_args!("{} bytes", self.root_certificate.len()),
            )
            .field("root_private_key", &"[redacted]")
            .field(
                "wifi_mac_address",
                &self.wifi_mac_address.as_ref().map(|_| "[redacted]"),
            )
            .field(
                "escrow_bag",
                &self.escrow_bag.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

impl PairRecord {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self> {
        let value = Value::from_reader(std::io::Cursor::new(bytes))
            .map_err(|error| Error::InvalidPairingRecord(error.to_string()))?;
        let dictionary = value.as_dictionary().ok_or_else(|| {
            Error::InvalidPairingRecord("pairing record root is not a dictionary".to_owned())
        })?;
        Ok(Self {
            host_id: string_field(dictionary, "HostID")?,
            system_buid: string_field(dictionary, "SystemBUID")?,
            host_certificate: data_field(dictionary, "HostCertificate")?,
            host_private_key: Zeroizing::new(data_field(dictionary, "HostPrivateKey")?),
            device_certificate: data_field(dictionary, "DeviceCertificate")?,
            root_certificate: data_field(dictionary, "RootCertificate")?,
            root_private_key: Zeroizing::new(data_field(dictionary, "RootPrivateKey")?),
            wifi_mac_address: optional_string_field(dictionary, "WiFiMACAddress")?,
            escrow_bag: optional_data_field(dictionary, "EscrowBag")?.map(Zeroizing::new),
        })
    }

    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    pub fn system_buid(&self) -> &str {
        &self.system_buid
    }

    pub fn host_certificate(&self) -> &[u8] {
        &self.host_certificate
    }

    pub fn host_private_key(&self) -> &[u8] {
        &self.host_private_key
    }

    pub fn device_certificate(&self) -> &[u8] {
        &self.device_certificate
    }

    pub(crate) fn set_escrow_bag(&mut self, escrow_bag: Option<Vec<u8>>) {
        self.escrow_bag = escrow_bag.map(Zeroizing::new);
    }

    pub(crate) fn pairing_dictionary(&self) -> Dictionary {
        self.dictionary(false)
    }

    fn persisted_dictionary(&self) -> Dictionary {
        self.dictionary(true)
    }

    fn dictionary(&self, include_private_keys: bool) -> Dictionary {
        let mut dictionary = Dictionary::new();
        dictionary.insert(
            "DeviceCertificate".to_owned(),
            Value::Data(self.device_certificate.clone()),
        );
        dictionary.insert(
            "HostCertificate".to_owned(),
            Value::Data(self.host_certificate.clone()),
        );
        dictionary.insert("HostID".to_owned(), Value::String(self.host_id.clone()));
        dictionary.insert(
            "RootCertificate".to_owned(),
            Value::Data(self.root_certificate.clone()),
        );
        dictionary.insert(
            "SystemBUID".to_owned(),
            Value::String(self.system_buid.clone()),
        );
        if let Some(wifi_mac_address) = &self.wifi_mac_address {
            dictionary.insert(
                "WiFiMACAddress".to_owned(),
                Value::String(wifi_mac_address.clone()),
            );
        }
        if include_private_keys {
            dictionary.insert(
                "RootPrivateKey".to_owned(),
                Value::Data(self.root_private_key.to_vec()),
            );
            dictionary.insert(
                "HostPrivateKey".to_owned(),
                Value::Data(self.host_private_key.to_vec()),
            );
            if let Some(escrow_bag) = &self.escrow_bag {
                dictionary.insert("EscrowBag".to_owned(), Value::Data(escrow_bag.to_vec()));
            }
        }
        dictionary
    }
}

#[derive(Debug, Clone)]
pub struct PairRecordStore {
    directory: PathBuf,
}

impl PairRecordStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn load(&self, device: &str) -> Result<PairRecord> {
        let stem = pair_record_stem(device)?;
        let identifiers = if stem == device {
            vec![stem, device.to_uppercase(), device.to_lowercase()]
        } else {
            vec![stem]
        };
        for identifier in identifiers {
            let path = self.directory.join(format!("{identifier}.plist"));
            match fs::metadata(&path) {
                Ok(metadata) if metadata.len() > MAX_PAIR_RECORD_BYTES => {
                    return Err(Error::InvalidPairingRecord(format!(
                        "pairing record is {} bytes; maximum is {MAX_PAIR_RECORD_BYTES}",
                        metadata.len()
                    )));
                }
                Ok(_) => {
                    let bytes = fs::read(&path)
                        .map_err(|source| Error::io("reading Lockdown pairing record", source))?;
                    return PairRecord::parse(&bytes);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(Error::io("inspecting Lockdown pairing record", source));
                }
            }
        }
        Err(Error::PairingRecordMissing {
            device: device.to_owned(),
            directory: self.directory.clone(),
        })
    }

    pub(crate) fn system_buid(&self) -> Result<String> {
        let path = self.directory.join(SYSTEM_CONFIGURATION_FILE);
        match fs::read(&path) {
            Ok(bytes) => return parse_system_buid(&bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(Error::io("reading Lockdown system configuration", source));
            }
        }

        Ok(self
            .oldest_pair_record_system_buid()?
            .unwrap_or_else(|| Uuid::new_v4().hyphenated().to_string().to_uppercase()))
    }

    fn oldest_pair_record_system_buid(&self) -> Result<Option<String>> {
        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(Error::io("inspecting Lockdown pairing directory", source));
            }
        };
        let mut records = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|source| Error::io("inspecting Lockdown pairing directory", source))?;
            let file_name = entry.file_name();
            if file_name == SYSTEM_CONFIGURATION_FILE
                || entry
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str())
                    != Some("plist")
            {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|source| Error::io("inspecting Lockdown pairing record", source))?;
            if !metadata.file_type().is_file() || metadata.len() > MAX_PAIR_RECORD_BYTES {
                continue;
            }
            let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            records.push((modified, file_name, entry.path()));
        }
        records.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
        for (_, _, path) in records {
            let bytes = fs::read(path)
                .map_err(|source| Error::io("reading Lockdown pairing record", source))?;
            if let Ok(record) = PairRecord::parse(&bytes) {
                return Ok(Some(record.system_buid().to_owned()));
            }
        }
        Ok(None)
    }

    fn save_system_buid(&self, system_buid: &str) -> Result<()> {
        fs::create_dir_all(&self.directory)
            .map_err(|source| Error::io("creating Lockdown pairing directory", source))?;
        set_mode(
            &self.directory,
            0o700,
            "securing Lockdown pairing directory",
        )?;
        let target = self.directory.join(SYSTEM_CONFIGURATION_FILE);
        let temporary = self.directory.join(format!(
            ".{SYSTEM_CONFIGURATION_FILE}.{}.tmp",
            Uuid::new_v4()
        ));
        let result = (|| {
            let mut file = secure_new_file(&temporary)?;
            Value::Dictionary(Dictionary::from_iter([(
                SYSTEM_BUID_KEY.to_owned(),
                Value::String(system_buid.to_owned()),
            )]))
            .to_writer_xml(&mut file)
            .map_err(|error| Error::InvalidPairingRecord(error.to_string()))?;
            file.flush()
                .map_err(|source| Error::io("flushing Lockdown system configuration", source))?;
            file.sync_all()
                .map_err(|source| Error::io("syncing Lockdown system configuration", source))?;
            drop(file);
            fs::rename(&temporary, &target)
                .map_err(|source| Error::io("installing Lockdown system configuration", source))?;
            set_mode(&target, 0o600, "securing Lockdown system configuration")?;
            File::open(&self.directory)
                .and_then(|directory| directory.sync_all())
                .map_err(|source| Error::io("syncing Lockdown pairing directory", source))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub(crate) fn save(&self, device: &str, record: &PairRecord) -> Result<()> {
        let stem = pair_record_stem(device)?;
        self.save_system_buid_if_missing(record.system_buid())?;
        fs::create_dir_all(&self.directory)
            .map_err(|source| Error::io("creating Lockdown pairing directory", source))?;
        set_mode(
            &self.directory,
            0o700,
            "securing Lockdown pairing directory",
        )?;

        let target = self.directory.join(format!("{stem}.plist"));
        let temporary = self
            .directory
            .join(format!(".{stem}.{}.tmp", Uuid::new_v4()));
        let result = (|| {
            let mut file = secure_new_file(&temporary)?;
            Value::Dictionary(record.persisted_dictionary())
                .to_writer_xml(&mut file)
                .map_err(|error| Error::InvalidPairingRecord(error.to_string()))?;
            file.flush()
                .map_err(|source| Error::io("flushing Lockdown pairing record", source))?;
            file.sync_all()
                .map_err(|source| Error::io("syncing Lockdown pairing record", source))?;
            drop(file);
            fs::rename(&temporary, &target)
                .map_err(|source| Error::io("installing Lockdown pairing record", source))?;
            set_mode(&target, 0o600, "securing Lockdown pairing record")?;
            File::open(&self.directory)
                .and_then(|directory| directory.sync_all())
                .map_err(|source| Error::io("syncing Lockdown pairing directory", source))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn save_system_buid_if_missing(&self, system_buid: &str) -> Result<()> {
        match fs::read(self.directory.join(SYSTEM_CONFIGURATION_FILE)) {
            Ok(bytes) => {
                parse_system_buid(&bytes)?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let authoritative = self
                    .oldest_pair_record_system_buid()?
                    .unwrap_or_else(|| system_buid.to_owned());
                self.save_system_buid(&authoritative)
            }
            Err(source) => Err(Error::io("reading Lockdown system configuration", source)),
        }
    }
}

pub(crate) fn generate_pair_record(
    device_public_key_pem: &[u8],
    wifi_mac_address: Option<String>,
    system_buid: String,
) -> Result<PairRecord> {
    generate_pair_record_with_rng(
        device_public_key_pem,
        wifi_mac_address,
        system_buid,
        &mut OsRng,
        SystemTime::now(),
    )
}

fn generate_pair_record_with_rng<R: CryptoRng + RngCore>(
    device_public_key_pem: &[u8],
    wifi_mac_address: Option<String>,
    system_buid: String,
    rng: &mut R,
    now: SystemTime,
) -> Result<PairRecord> {
    let device_public_key = decode_rsa_public_key(device_public_key_pem)?;
    if device_public_key.size() < 256 {
        return Err(Error::InvalidPairingRecord(
            "iPhone DevicePublicKey is smaller than RSA-2048".to_owned(),
        ));
    }
    let root_key = RsaPrivateKey::new(rng, 2048).map_err(|error| {
        Error::InvalidPairingRecord(format!("root key generation failed: {error}"))
    })?;
    let host_key = RsaPrivateKey::new(rng, 2048).map_err(|error| {
        Error::InvalidPairingRecord(format!("host key generation failed: {error}"))
    })?;
    let signer = SigningKey::<Sha256>::new(root_key.clone());
    let validity = validity(now)?;
    let empty_name = Name::default();

    let root_certificate = build_certificate(
        Profile::Manual { issuer: None },
        public_key_info(&RsaPublicKey::from(&root_key))?,
        &signer,
        validity,
        CertificateKind::Root,
    )?;
    let host_certificate = build_certificate(
        Profile::Manual {
            issuer: Some(empty_name.clone()),
        },
        public_key_info(&RsaPublicKey::from(&host_key))?,
        &signer,
        validity,
        CertificateKind::Host,
    )?;
    let device_certificate = build_certificate(
        Profile::Manual {
            issuer: Some(empty_name),
        },
        public_key_info(&device_public_key)?,
        &signer,
        validity,
        CertificateKind::Device,
    )?;

    Ok(PairRecord {
        host_id: Uuid::new_v4().hyphenated().to_string().to_uppercase(),
        system_buid,
        host_certificate,
        host_private_key: Zeroizing::new(private_key_pem(&host_key)?),
        device_certificate,
        root_certificate,
        root_private_key: Zeroizing::new(private_key_pem(&root_key)?),
        wifi_mac_address,
        escrow_bag: None,
    })
}

fn parse_system_buid(bytes: &[u8]) -> Result<String> {
    let value = Value::from_reader(std::io::Cursor::new(bytes))
        .map_err(|error| Error::InvalidPairingRecord(error.to_string()))?;
    let dictionary = value.as_dictionary().ok_or_else(|| {
        Error::InvalidPairingRecord("system configuration root is not a dictionary".to_owned())
    })?;
    string_field(dictionary, SYSTEM_BUID_KEY)
}

#[derive(Debug, Clone, Copy)]
enum CertificateKind {
    Root,
    Host,
    Device,
}

fn build_certificate(
    profile: Profile,
    subject_public_key: SubjectPublicKeyInfoOwned,
    signer: &SigningKey<Sha256>,
    validity: Validity,
    kind: CertificateKind,
) -> Result<Vec<u8>> {
    let mut builder = CertificateBuilder::new(
        profile,
        SerialNumber::from(1_u32),
        validity,
        Name::default(),
        subject_public_key.clone(),
        signer,
    )
    .map_err(pairing_crypto_error)?;
    builder
        .add_extension(&BasicConstraints {
            ca: matches!(kind, CertificateKind::Root),
            path_len_constraint: None,
        })
        .map_err(pairing_crypto_error)?;
    if !matches!(kind, CertificateKind::Root) {
        builder
            .add_extension(&KeyUsage(
                KeyUsages::DigitalSignature | KeyUsages::KeyEncipherment,
            ))
            .map_err(pairing_crypto_error)?;
    }
    if matches!(kind, CertificateKind::Device) {
        let identifier = SubjectKeyIdentifier::try_from(subject_public_key.owned_to_ref())
            .map_err(pairing_crypto_error)?;
        builder
            .add_extension(&identifier)
            .map_err(pairing_crypto_error)?;
    }
    let certificate: Certificate = builder.build::<Signature>().map_err(pairing_crypto_error)?;
    certificate
        .to_pem(LineEnding::LF)
        .map(String::into_bytes)
        .map_err(|error| {
            Error::InvalidPairingRecord(format!("certificate encoding failed: {error}"))
        })
}

fn validity(now: SystemTime) -> Result<Validity> {
    let not_before = now.checked_sub(CLOCK_SKEW).ok_or_else(|| {
        Error::InvalidPairingRecord("system clock cannot represent certificate start".to_owned())
    })?;
    let not_after = now.checked_add(CERTIFICATE_VALIDITY).ok_or_else(|| {
        Error::InvalidPairingRecord("system clock cannot represent certificate expiry".to_owned())
    })?;
    Ok(Validity {
        not_before: Time::try_from(not_before).map_err(pairing_der_error)?,
        not_after: Time::try_from(not_after).map_err(pairing_der_error)?,
    })
}

fn decode_rsa_public_key(pem: &[u8]) -> Result<RsaPublicKey> {
    let pem = std::str::from_utf8(pem).map_err(|_| {
        Error::InvalidPairingRecord("iPhone DevicePublicKey is not PEM text".to_owned())
    })?;
    RsaPublicKey::from_public_key_pem(pem)
        .or_else(|_| RsaPublicKey::from_pkcs1_pem(pem))
        .map_err(|_| {
            Error::InvalidPairingRecord(
                "iPhone DevicePublicKey is not an RSA public key".to_owned(),
            )
        })
}

fn public_key_info(key: &RsaPublicKey) -> Result<SubjectPublicKeyInfoOwned> {
    let der = key.to_public_key_der().map_err(pairing_der_error)?;
    SubjectPublicKeyInfoOwned::try_from(der.as_bytes()).map_err(pairing_der_error)
}

fn private_key_pem(key: &RsaPrivateKey) -> Result<Vec<u8>> {
    key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
        .map(|pem| pem.as_bytes().to_vec())
        .map_err(pairing_der_error)
}

fn pairing_crypto_error(error: impl std::fmt::Display) -> Error {
    Error::InvalidPairingRecord(format!("pairing certificate generation failed: {error}"))
}

fn pairing_der_error(error: impl std::fmt::Display) -> Error {
    Error::InvalidPairingRecord(format!("pairing key encoding failed: {error}"))
}

fn pair_record_stem(device: &str) -> Result<String> {
    if device.is_empty() || device.len() > MAX_DEVICE_IDENTIFIER_BYTES {
        return Err(Error::InvalidPairingRecord(
            "device identifier is empty or exceeds the storage limit".to_owned(),
        ));
    }
    if device
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Ok(device.to_owned());
    }

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut stem = String::with_capacity(4 + device.len() * 2);
    stem.push_str("usb-");
    for byte in device.bytes() {
        stem.push(HEX[usize::from(byte >> 4)] as char);
        stem.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    Ok(stem)
}

#[cfg(unix)]
fn secure_new_file(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|source| Error::io("creating Lockdown pairing record", source))
}

#[cfg(not(unix))]
fn secure_new_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| Error::io("creating Lockdown pairing record", source))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32, operation: &'static str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|source| Error::io(operation, source))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32, _operation: &'static str) -> Result<()> {
    Ok(())
}

fn string_field(dictionary: &Dictionary, key: &'static str) -> Result<String> {
    dictionary
        .get(key)
        .and_then(Value::as_string)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::InvalidPairingRecord(format!("{key} is missing or empty")))
}

fn optional_string_field(dictionary: &Dictionary, key: &'static str) -> Result<Option<String>> {
    match dictionary.get(key) {
        None => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        Some(_) => Err(Error::InvalidPairingRecord(format!(
            "{key} has an invalid type or is empty"
        ))),
    }
}

fn data_field(dictionary: &Dictionary, key: &'static str) -> Result<Vec<u8>> {
    dictionary
        .get(key)
        .and_then(Value::as_data)
        .filter(|value| !value.is_empty())
        .map(<[u8]>::to_vec)
        .ok_or_else(|| Error::InvalidPairingRecord(format!("{key} is missing or empty")))
}

fn optional_data_field(dictionary: &Dictionary, key: &'static str) -> Result<Option<Vec<u8>>> {
    match dictionary.get(key) {
        None => Ok(None),
        Some(Value::Data(value)) if !value.is_empty() => Ok(Some(value.clone())),
        Some(_) => Err(Error::InvalidPairingRecord(format!(
            "{key} has an invalid type or is empty"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use der::{DecodePem, Encode};
    use rand::{SeedableRng, rngs::StdRng};
    use rsa::{
        pkcs1::EncodeRsaPublicKey,
        pkcs1v15::VerifyingKey,
        pkcs8::{DecodePrivateKey, EncodePublicKey},
        signature::Verifier,
    };

    use super::*;

    fn device_key() -> (RsaPrivateKey, Vec<u8>) {
        let mut rng = StdRng::seed_from_u64(7);
        let key = RsaPrivateKey::new(&mut rng, 2048).expect("device key");
        let pem = RsaPublicKey::from(&key)
            .to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
            .expect("public pem")
            .into_bytes();
        (key, pem)
    }

    fn dummy_record() -> PairRecord {
        PairRecord {
            host_id: "host-id-marker".to_owned(),
            system_buid: "system-buid-marker".to_owned(),
            host_certificate: b"host-certificate".to_vec(),
            host_private_key: Zeroizing::new(b"host-private-key-marker".to_vec()),
            device_certificate: b"device-certificate".to_vec(),
            root_certificate: b"root-certificate".to_vec(),
            root_private_key: Zeroizing::new(b"root-private-key-marker".to_vec()),
            wifi_mac_address: None,
            escrow_bag: None,
        }
    }

    #[test]
    fn generated_certificates_have_expected_key_relationships() {
        let (device_key, device_pem) = device_key();
        let mut rng = StdRng::seed_from_u64(9);
        let record = generate_pair_record_with_rng(
            &device_pem,
            Some("00:11:22:33:44:55".to_owned()),
            "persistent-system-buid".to_owned(),
            &mut rng,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        )
        .expect("pair record");

        let root = Certificate::from_pem(&record.root_certificate).expect("root cert");
        let host = Certificate::from_pem(&record.host_certificate).expect("host cert");
        let device = Certificate::from_pem(&record.device_certificate).expect("device cert");
        assert_eq!(root.tbs_certificate.subject, root.tbs_certificate.issuer);
        assert_eq!(host.tbs_certificate.issuer, root.tbs_certificate.subject);
        assert_eq!(device.tbs_certificate.issuer, root.tbs_certificate.subject);

        let host_key = RsaPrivateKey::from_pkcs8_pem(
            std::str::from_utf8(&record.host_private_key).expect("host key utf8"),
        )
        .expect("host key");
        let host_spki = RsaPublicKey::from(&host_key)
            .to_public_key_der()
            .expect("host spki");
        assert_eq!(
            host.tbs_certificate
                .subject_public_key_info
                .to_der()
                .expect("host cert spki"),
            host_spki.as_bytes()
        );
        let device_spki = RsaPublicKey::from(&device_key)
            .to_public_key_der()
            .expect("device spki");
        assert_eq!(
            device
                .tbs_certificate
                .subject_public_key_info
                .to_der()
                .expect("device cert spki"),
            device_spki.as_bytes()
        );
        assert_eq!(host.signature_algorithm, root.signature_algorithm);
        assert_eq!(device.signature_algorithm, root.signature_algorithm);
        let root_public = RsaPublicKey::from_public_key_der(
            &root
                .tbs_certificate
                .subject_public_key_info
                .to_der()
                .expect("root spki"),
        )
        .expect("root public key");
        verify_certificate_signature(&root, &root_public);
        verify_certificate_signature(&host, &root_public);
        verify_certificate_signature(&device, &root_public);
    }

    #[test]
    fn atomic_store_uses_private_permissions_and_round_trips_escrow_bag() {
        let mut record = dummy_record();
        record.set_escrow_bag(Some(vec![4, 5, 6]));
        let temporary = tempfile::tempdir().expect("tempdir");
        let directory = temporary.path().join("lockdown");
        let store = PairRecordStore::new(&directory);
        store.save("device-id", &record).expect("save");

        assert_eq!(
            fs::metadata(&directory)
                .expect("directory")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let path = directory.join("device-id.plist");
        assert_eq!(
            fs::metadata(&path).expect("record").permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(&directory).expect("entries").count(), 2);
        let loaded = store.load("device-id").expect("load");
        assert_eq!(
            loaded.escrow_bag.as_ref().map(|bag| bag.as_slice()),
            Some([4, 5, 6].as_slice())
        );
    }

    #[test]
    fn non_path_safe_identifier_uses_a_deterministic_encoded_filename() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let directory = temporary.path().join("lockdown");
        let store = PairRecordStore::new(&directory);
        let identifier = "device/id\nmarker";
        store
            .save(identifier, &dummy_record())
            .expect("encoded save");

        let expected = format!("{}.plist", pair_record_stem(identifier).expect("stem"));
        let mut entries = fs::read_dir(&directory)
            .expect("entries")
            .map(|entry| entry.expect("entry").file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(
            entries,
            [
                std::ffi::OsString::from(SYSTEM_CONFIGURATION_FILE),
                std::ffi::OsString::from(expected),
            ]
        );
        store.load(identifier).expect("encoded load");
        assert!(!temporary.path().join("device").exists());
    }

    #[test]
    fn normal_identifier_keeps_the_legacy_pair_record_filename() {
        assert_eq!(
            pair_record_stem("0000811000093062360A401E").expect("stem"),
            "0000811000093062360A401E"
        );
        assert!(pair_record_stem("").is_err());
        assert!(pair_record_stem(&"x".repeat(MAX_DEVICE_IDENTIFIER_BYTES + 1)).is_err());
    }

    #[test]
    fn pairing_request_has_the_protocol_compatible_golden_key_set() {
        let record = dummy_record();
        let dictionary = record.pairing_dictionary();
        assert_eq!(
            dictionary.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "DeviceCertificate",
                "HostCertificate",
                "HostID",
                "RootCertificate",
                "SystemBUID",
            ]
        );
        assert!(!dictionary.contains_key("HostPrivateKey"));
        assert!(!dictionary.contains_key("RootPrivateKey"));
        let mut serialized = Vec::new();
        Value::Dictionary(dictionary)
            .to_writer_xml(&mut serialized)
            .expect("request plist");
        let serialized = String::from_utf8(serialized).expect("request text");
        assert!(!serialized.contains("HostPrivateKey"));
        assert!(!serialized.contains("RootPrivateKey"));
    }

    #[test]
    fn system_buid_is_persistent_and_migrates_the_oldest_pair_record() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let directory = temporary.path().join("lockdown");
        let store = PairRecordStore::new(&directory);
        fs::create_dir_all(&directory).expect("pairing directory");
        Value::Dictionary(dummy_record().persisted_dictionary())
            .to_file_xml(directory.join("first-device.plist"))
            .expect("existing pair record");

        store
            .save_system_buid_if_missing("new-device-system-buid")
            .expect("migrate system configuration");

        let first = store
            .system_buid()
            .expect("read migrated system configuration");
        let second = store.system_buid().expect("reload system configuration");
        assert_eq!(first, "system-buid-marker");
        assert_eq!(second, first);

        let configuration = Value::from_file(directory.join(SYSTEM_CONFIGURATION_FILE))
            .expect("system configuration");
        assert_eq!(
            configuration
                .as_dictionary()
                .and_then(|dictionary| dictionary.get(SYSTEM_BUID_KEY))
                .and_then(Value::as_string),
            Some("system-buid-marker")
        );
        assert_eq!(
            fs::metadata(directory.join(SYSTEM_CONFIGURATION_FILE))
                .expect("system configuration metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn debug_never_contains_private_key_or_escrow_material() {
        let mut record = dummy_record();
        record.set_escrow_bag(Some(b"secret-escrow-marker".to_vec()));
        let host_id = record.host_id.clone();
        let system_buid = record.system_buid.clone();
        let debug = format!("{record:?}");
        assert!(!debug.contains("PRIVATE KEY"));
        assert!(!debug.contains("secret-escrow-marker"));
        assert!(!debug.contains(&host_id));
        assert!(!debug.contains(&system_buid));
        assert!(debug.contains("[redacted]"));
    }

    #[test]
    fn failed_atomic_install_removes_its_temporary_file() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let directory = temporary.path().join("lockdown");
        fs::create_dir_all(directory.join("device-id.plist")).expect("blocking target");
        let store = PairRecordStore::new(&directory);
        assert!(store.save("device-id", &dummy_record()).is_err());
        let mut entries = fs::read_dir(&directory)
            .expect("entries")
            .map(|entry| entry.expect("entry").file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(
            entries,
            [
                std::ffi::OsString::from(SYSTEM_CONFIGURATION_FILE),
                std::ffi::OsString::from("device-id.plist"),
            ]
        );
    }

    fn verify_certificate_signature(certificate: &Certificate, root_public: &RsaPublicKey) {
        let signature =
            Signature::try_from(certificate.signature.raw_bytes()).expect("certificate signature");
        VerifyingKey::<Sha256>::new(root_public.clone())
            .verify(
                &certificate
                    .tbs_certificate
                    .to_der()
                    .expect("certificate body"),
                &signature,
            )
            .expect("root signature");
    }
}
