use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use ed25519_dalek::{SigningKey, VerifyingKey};
use fs2::FileExt;
use rand_core::{CryptoRng, OsRng, RngCore};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::{Error, Result, crypto};

const IDENTITY_FILE: &str = "identity.json";
const PAIRINGS_FILE: &str = "pairings.json";
const LOCK_FILE: &str = ".pairing.lock";
const STORAGE_VERSION: u8 = 1;
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairingStoreLimits {
    pub maximum_file_bytes: usize,
    pub maximum_pairings: usize,
    pub maximum_identifier_bytes: usize,
}

impl Default for PairingStoreLimits {
    fn default() -> Self {
        Self {
            maximum_file_bytes: 1024 * 1024,
            maximum_pairings: 256,
            maximum_identifier_bytes: 256,
        }
    }
}

impl PairingStoreLimits {
    fn validate(self) -> Result<Self> {
        if self.maximum_file_bytes < 128
            || self.maximum_pairings == 0
            || self.maximum_identifier_bytes == 0
        {
            return Err(Error::Storage("invalid pairing-store limits".to_owned()));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone)]
pub struct PairingStore {
    directory: PathBuf,
    limits: PairingStoreLimits,
}

impl PairingStore {
    pub fn open(directory: impl Into<PathBuf>) -> Result<Self> {
        Self::with_limits(directory, PairingStoreLimits::default())
    }

    pub fn with_limits(directory: impl Into<PathBuf>, limits: PairingStoreLimits) -> Result<Self> {
        let store = Self {
            directory: directory.into(),
            limits: limits.validate()?,
        };
        store.ensure_directory()?;
        Ok(store)
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn load_or_create_identity(&self) -> Result<AccessoryIdentity> {
        self.load_or_create_identity_with_rng(&mut OsRng)
    }

    pub fn load_or_create_identity_with_rng<R>(&self, random: &mut R) -> Result<AccessoryIdentity>
    where
        R: RngCore + CryptoRng,
    {
        self.with_exclusive_lock(|| {
            let path = self.directory.join(IDENTITY_FILE);
            match self.read_file(&path)? {
                Some(encoded) => decode_identity(&encoded, self.limits),
                None => {
                    let signing_key = SigningKey::generate(random);
                    let mut uuid_bytes = [0; 16];
                    random.fill_bytes(&mut uuid_bytes);
                    uuid_bytes[6] = (uuid_bytes[6] & 0x0f) | 0x40;
                    uuid_bytes[8] = (uuid_bytes[8] & 0x3f) | 0x80;
                    let identity = AccessoryIdentity {
                        pairing_id: Uuid::from_bytes(uuid_bytes).to_string(),
                        signing_key,
                    };
                    let record = IdentityRecord {
                        version: STORAGE_VERSION,
                        pairing_id: identity.pairing_id.clone(),
                        private_key: hex::encode(identity.signing_key.to_bytes()),
                        public_key: hex::encode(identity.public_key()),
                    };
                    let encoded = Zeroizing::new(serde_json::to_vec(&record).map_err(|error| {
                        Error::Storage(format!("encode accessory identity: {error}"))
                    })?);
                    self.atomic_write(&path, &encoded)?;
                    Ok(identity)
                }
            }
        })
    }

    pub fn save_pairing(&self, identifier: &str, public_key: [u8; 32]) -> Result<()> {
        validate_identifier(identifier, self.limits)?;
        VerifyingKey::from_bytes(&public_key)
            .map_err(|_| Error::Storage("controller Ed25519 public key is invalid".to_owned()))?;
        self.with_exclusive_lock(|| {
            let path = self.directory.join(PAIRINGS_FILE);
            let mut pairings = self.load_pairings_unlocked(&path)?;
            if !pairings.contains_key(identifier) && pairings.len() >= self.limits.maximum_pairings
            {
                return Err(Error::Storage(format!(
                    "pairing store already contains the maximum {} controllers",
                    self.limits.maximum_pairings
                )));
            }
            pairings.insert(identifier.to_owned(), hex::encode(public_key));
            let record = PairingsRecord {
                version: STORAGE_VERSION,
                pairings,
            };
            let encoded = serde_json::to_vec(&record)
                .map_err(|error| Error::Storage(format!("encode controller pairings: {error}")))?;
            self.atomic_write(&path, &encoded)
        })
    }

    pub fn pairing(&self, identifier: &str) -> Result<Option<[u8; 32]>> {
        validate_identifier(identifier, self.limits)?;
        self.with_exclusive_lock(|| {
            let path = self.directory.join(PAIRINGS_FILE);
            let pairings = self.load_pairings_unlocked(&path)?;
            pairings
                .get(identifier)
                .map(|encoded| decode_public_key(encoded, "stored controller"))
                .transpose()
        })
    }

    fn load_pairings_unlocked(&self, path: &Path) -> Result<BTreeMap<String, String>> {
        let Some(encoded) = self.read_file(path)? else {
            return Ok(BTreeMap::new());
        };
        let record: PairingsRecord = serde_json::from_slice(&encoded)
            .map_err(|error| Error::Storage(format!("decode controller pairings: {error}")))?;
        if record.version != STORAGE_VERSION {
            return Err(Error::Storage(format!(
                "unsupported pairing-store version {}",
                record.version
            )));
        }
        if record.pairings.len() > self.limits.maximum_pairings {
            return Err(Error::Storage(format!(
                "pairing store contains {} controllers; maximum is {}",
                record.pairings.len(),
                self.limits.maximum_pairings
            )));
        }
        for (identifier, public_key) in &record.pairings {
            validate_identifier(identifier, self.limits)?;
            decode_public_key(public_key, "stored controller")?;
        }
        Ok(record.pairings)
    }

    fn ensure_directory(&self) -> Result<()> {
        if let Ok(metadata) = fs::symlink_metadata(&self.directory) {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(Error::Storage(format!(
                    "{} is not a real directory",
                    self.directory.display()
                )));
            }
        } else {
            fs::create_dir_all(&self.directory).map_err(|error| {
                Error::Storage(format!("create {}: {error}", self.directory.display()))
            })?;
        }
        set_directory_permissions(&self.directory)
    }

    fn with_exclusive_lock<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        self.ensure_directory()?;
        let lock_path = self.directory.join(LOCK_FILE);
        reject_symlink_if_present(&lock_path)?;
        let lock = secure_open_for_lock(&lock_path)?;
        lock.lock_exclusive()
            .map_err(|error| Error::Storage(format!("lock {}: {error}", lock_path.display())))?;
        operation()
    }

    fn read_file(&self, path: &Path) -> Result<Option<Zeroizing<Vec<u8>>>> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(Error::Storage(format!(
                    "inspect {}: {error}",
                    path.display()
                )));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(Error::Storage(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        require_file_permissions(path, &metadata)?;
        let declared = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        if declared > self.limits.maximum_file_bytes {
            return Err(Error::Storage(format!(
                "{} is {declared} bytes; maximum is {}",
                path.display(),
                self.limits.maximum_file_bytes
            )));
        }
        let mut file = File::open(path)
            .map_err(|error| Error::Storage(format!("open {}: {error}", path.display())))?;
        let mut encoded = Zeroizing::new(Vec::with_capacity(declared));
        Read::by_ref(&mut file)
            .take(u64::try_from(self.limits.maximum_file_bytes + 1).unwrap_or(u64::MAX))
            .read_to_end(&mut encoded)
            .map_err(|error| Error::Storage(format!("read {}: {error}", path.display())))?;
        if encoded.len() > self.limits.maximum_file_bytes {
            return Err(Error::Storage(format!(
                "{} grew beyond the configured file bound",
                path.display()
            )));
        }
        Ok(Some(encoded))
    }

    fn atomic_write(&self, path: &Path, encoded: &[u8]) -> Result<()> {
        if encoded.len() > self.limits.maximum_file_bytes {
            return Err(Error::Storage(format!(
                "encoded pairing record is {} bytes; maximum is {}",
                encoded.len(),
                self.limits.maximum_file_bytes
            )));
        }
        reject_symlink_if_present(path)?;
        let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = self.directory.join(format!(
            ".{}.tmp-{}-{sequence}",
            path.file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| Error::Storage("storage filename is not UTF-8".to_owned()))?,
            std::process::id()
        ));
        let write_result = (|| {
            let mut file = secure_create_new(&temporary)?;
            file.write_all(encoded).map_err(|error| {
                Error::Storage(format!("write {}: {error}", temporary.display()))
            })?;
            file.sync_all().map_err(|error| {
                Error::Storage(format!("sync {}: {error}", temporary.display()))
            })?;
            fs::rename(&temporary, path).map_err(|error| {
                Error::Storage(format!(
                    "replace {} with {}: {error}",
                    path.display(),
                    temporary.display()
                ))
            })?;
            File::open(&self.directory)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| {
                    Error::Storage(format!("sync {}: {error}", self.directory.display()))
                })
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result
    }
}

pub struct AccessoryIdentity {
    pairing_id: String,
    signing_key: SigningKey,
}

impl std::fmt::Debug for AccessoryIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccessoryIdentity")
            .field("pairing_id", &self.pairing_id)
            .field("signing_key", &"[redacted]")
            .field("public_key", &hex::encode(self.public_key()))
            .finish()
    }
}

impl AccessoryIdentity {
    pub fn pairing_id(&self) -> &str {
        &self.pairing_id
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    pub(crate) fn sign(&self, message: &[u8]) -> [u8; 64] {
        crypto::sign(&self.signing_key, message)
    }
}

#[derive(Debug, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
struct IdentityRecord {
    version: u8,
    pairing_id: String,
    private_key: String,
    public_key: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct PairingsRecord {
    version: u8,
    pairings: BTreeMap<String, String>,
}

fn decode_identity(encoded: &[u8], limits: PairingStoreLimits) -> Result<AccessoryIdentity> {
    let record: IdentityRecord = serde_json::from_slice(encoded)
        .map_err(|error| Error::Storage(format!("decode accessory identity: {error}")))?;
    if record.version != STORAGE_VERSION {
        return Err(Error::Storage(format!(
            "unsupported accessory-identity version {}",
            record.version
        )));
    }
    validate_identifier(&record.pairing_id, limits)?;
    let private = Zeroizing::new(
        hex::decode(&record.private_key)
            .map_err(|_| Error::Storage("accessory private key is not hex".to_owned()))?,
    );
    let private: [u8; 32] = private
        .as_slice()
        .try_into()
        .map_err(|_| Error::Storage("accessory private key is not 32 bytes".to_owned()))?;
    let signing_key = SigningKey::from_bytes(&private);
    let public_key = decode_public_key(&record.public_key, "accessory")?;
    if signing_key.verifying_key().to_bytes() != public_key {
        return Err(Error::Storage(
            "accessory public key does not match its private key".to_owned(),
        ));
    }
    Ok(AccessoryIdentity {
        pairing_id: record.pairing_id.clone(),
        signing_key,
    })
}

fn decode_public_key(encoded: &str, owner: &str) -> Result<[u8; 32]> {
    let decoded = hex::decode(encoded)
        .map_err(|_| Error::Storage(format!("{owner} public key is not hex")))?;
    let public_key: [u8; 32] = decoded
        .as_slice()
        .try_into()
        .map_err(|_| Error::Storage(format!("{owner} public key is not 32 bytes")))?;
    VerifyingKey::from_bytes(&public_key)
        .map_err(|_| Error::Storage(format!("{owner} Ed25519 public key is invalid")))?;
    Ok(public_key)
}

fn validate_identifier(identifier: &str, limits: PairingStoreLimits) -> Result<()> {
    if identifier.is_empty()
        || identifier.len() > limits.maximum_identifier_bytes
        || identifier.bytes().any(|byte| byte == 0)
        || identifier.chars().any(char::is_control)
    {
        return Err(Error::Storage(
            "pairing identifier is empty, too long, or contains a control byte".to_owned(),
        ));
    }
    Ok(())
}

fn reject_symlink_if_present(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::Storage(format!(
            "refusing symbolic-link storage path {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Storage(format!(
            "inspect {}: {error}",
            path.display()
        ))),
    }
}

#[cfg(unix)]
fn secure_create_new(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| Error::Storage(format!("create {}: {error}", path.display())))
}

#[cfg(not(unix))]
fn secure_create_new(_path: &Path) -> Result<File> {
    Err(Error::Unsupported(
        "secure pairing storage requires Unix permission semantics",
    ))
}

#[cfg(unix)]
fn secure_open_for_lock(path: &Path) -> Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| Error::Storage(format!("open {}: {error}", path.display())))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| Error::Storage(format!("secure {}: {error}", path.display())))?;
    Ok(file)
}

#[cfg(not(unix))]
fn secure_open_for_lock(_path: &Path) -> Result<File> {
    Err(Error::Unsupported(
        "secure pairing storage requires Unix permission semantics",
    ))
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| Error::Storage(format!("secure {}: {error}", path.display())))?;
    let mode = fs::metadata(path)
        .map_err(|error| Error::Storage(format!("inspect {}: {error}", path.display())))?
        .permissions()
        .mode()
        & 0o777;
    if mode != 0o700 {
        return Err(Error::Storage(format!(
            "{} has mode {mode:04o}; required 0700",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path) -> Result<()> {
    Err(Error::Unsupported(
        "secure pairing storage requires Unix permission semantics",
    ))
}

#[cfg(unix)]
fn require_file_permissions(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o600 {
        return Err(Error::Storage(format!(
            "{} has mode {mode:04o}; required 0600",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn require_file_permissions(_path: &Path, _metadata: &fs::Metadata) -> Result<()> {
    Err(Error::Unsupported(
        "secure pairing storage requires Unix permission semantics",
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rand_core::Error as RngError;
    use tempfile::TempDir;

    use super::*;

    #[derive(Debug)]
    struct FixedRng(u8);

    impl RngCore for FixedRng {
        fn next_u32(&mut self) -> u32 {
            u32::from(self.0).wrapping_mul(0x0101_0101)
        }

        fn next_u64(&mut self) -> u64 {
            u64::from(self.next_u32()).wrapping_mul(0x0000_0001_0000_0001)
        }

        fn fill_bytes(&mut self, destination: &mut [u8]) {
            destination.fill(self.0);
            self.0 = self.0.wrapping_add(1);
        }

        fn try_fill_bytes(&mut self, destination: &mut [u8]) -> std::result::Result<(), RngError> {
            self.fill_bytes(destination);
            Ok(())
        }
    }

    impl CryptoRng for FixedRng {}

    #[test]
    fn identity_and_pairings_are_atomic_stable_and_restrictive() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = TempDir::new().expect("temporary");
        let path = temporary.path().join("cp");
        let store = PairingStore::open(&path).expect("store");
        let identity = store
            .load_or_create_identity_with_rng(&mut FixedRng(7))
            .expect("identity");
        let public = identity.public_key();
        let pairing_id = identity.pairing_id().to_owned();
        drop(identity);
        let loaded = store.load_or_create_identity().expect("reload");
        assert_eq!(loaded.public_key(), public);
        assert_eq!(loaded.pairing_id(), pairing_id);

        let controller = SigningKey::from_bytes(&[9; 32]).verifying_key().to_bytes();
        store.save_pairing("phone-1", controller).expect("save");
        assert_eq!(store.pairing("phone-1").expect("lookup"), Some(controller));

        assert_eq!(
            fs::metadata(&path).expect("directory").permissions().mode() & 0o777,
            0o700
        );
        for name in [IDENTITY_FILE, PAIRINGS_FILE, LOCK_FILE] {
            assert_eq!(
                fs::metadata(path.join(name))
                    .expect("file")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(fs::read_dir(&path).expect("entries").all(|entry| {
            !entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));
    }

    #[test]
    fn corrupted_identity_is_not_silently_replaced() {
        let temporary = TempDir::new().expect("temporary");
        let store = PairingStore::open(temporary.path()).expect("store");
        let identity = temporary.path().join(IDENTITY_FILE);
        fs::write(&identity, b"not-json").expect("write corrupt");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&identity, fs::Permissions::from_mode(0o600)).expect("permissions");
        }
        assert!(matches!(
            store.load_or_create_identity(),
            Err(Error::Storage(_))
        ));
        assert_eq!(fs::read(&identity).expect("unchanged"), b"not-json");
    }

    #[test]
    fn store_can_be_shared_between_pairing_state_machines() {
        let temporary = TempDir::new().expect("temporary");
        let store = Arc::new(PairingStore::open(temporary.path()).expect("store"));
        let first = Arc::clone(&store);
        let second = Arc::clone(&store);
        let controller = SigningKey::from_bytes(&[4; 32]).verifying_key().to_bytes();
        first.save_pairing("phone", controller).expect("save");
        assert_eq!(second.pairing("phone").expect("load"), Some(controller));
    }
}
