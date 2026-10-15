use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

use prelude_wireless::bluez::PhoneKind;

const PHONES_FILE: &str = "phones";
const LEGACY_LAST_PHONE_FILE: &str = "last-phone";
const MAXIMUM_PHONES_BYTES: u64 = 4096;
const MAXIMUM_NAME_CHARS: usize = 48;
pub const MAXIMUM_KNOWN_PHONES: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownPhone {
    pub kind: PhoneKind,
    pub address: String,
    pub name: String,
}

pub fn load(state_dir: &Path) -> Vec<KnownPhone> {
    match read_private(&state_dir.join(PHONES_FILE)) {
        Some(contents) => contents.lines().filter_map(parse_line).collect(),
        None => read_private(&state_dir.join(LEGACY_LAST_PHONE_FILE))
            .and_then(|contents| parse_line(contents.trim()))
            .into_iter()
            .collect(),
    }
}

pub fn remember(
    state_dir: &Path,
    kind: PhoneKind,
    address: &str,
    name: Option<&str>,
) -> io::Result<()> {
    if !is_bluetooth_address(address) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a Bluetooth address",
        ));
    }
    let mut phones = load(state_dir);
    let index = phones.iter().position(|phone| phone.address == address);
    let previous = index.map(|index| phones.remove(index));
    let name = name
        .map(sanitize_name)
        .filter(|name| !name.is_empty())
        .or_else(|| previous.as_ref().map(|phone| phone.name.clone()))
        .unwrap_or_else(|| address.to_owned());
    let phone = KnownPhone {
        kind,
        address: address.to_owned(),
        name,
    };
    if index == Some(0) && previous.as_ref() == Some(&phone) {
        return Ok(());
    }
    phones.insert(0, phone);
    phones.truncate(MAXIMUM_KNOWN_PHONES);
    store(state_dir, &phones)
}

pub fn promote(state_dir: &Path, address: &str) -> io::Result<Option<KnownPhone>> {
    let mut phones = load(state_dir);
    let Some(index) = phones.iter().position(|phone| phone.address == address) else {
        return Ok(None);
    };
    let phone = phones.remove(index);
    phones.insert(0, phone.clone());
    store(state_dir, &phones)?;
    Ok(Some(phone))
}

pub fn forget(state_dir: &Path, address: &str) -> io::Result<()> {
    let mut phones = load(state_dir);
    phones.retain(|phone| phone.address != address);
    store(state_dir, &phones)
}

fn store(state_dir: &Path, phones: &[KnownPhone]) -> io::Result<()> {
    fs::create_dir_all(state_dir)?;
    let path = state_dir.join(PHONES_FILE);
    let staged = state_dir.join(format!(".{PHONES_FILE}.tmp.{}", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&staged)?;
        for phone in phones {
            writeln!(
                file,
                "{} {} {}",
                phone.kind.as_str(),
                phone.address,
                phone.name
            )?;
        }
        file.sync_all()?;
        fs::rename(&staged, &path)?;
        fs::File::open(state_dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    } else {
        let _ = fs::remove_file(state_dir.join(LEGACY_LAST_PHONE_FILE));
    }
    result
}

fn read_private(path: &Path) -> Option<String> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file()
        || metadata.len() > MAXIMUM_PHONES_BYTES
        || metadata.permissions().mode() & 0o022 != 0
    {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn parse_line(line: &str) -> Option<KnownPhone> {
    let mut fields = line.splitn(3, ' ');
    let kind = PhoneKind::parse(fields.next()?)?;
    let address = fields.next()?;
    if !is_bluetooth_address(address) {
        return None;
    }
    let name = fields
        .next()
        .map(sanitize_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| address.to_owned());
    Some(KnownPhone {
        kind,
        address: address.to_owned(),
        name,
    })
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(MAXIMUM_NAME_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

fn is_bluetooth_address(text: &str) -> bool {
    text.len() == 17
        && text.split(':').count() == 6
        && text
            .split(':')
            .all(|octet| octet.len() == 2 && octet.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const IPHONE: &str = "A8:AB:B5:AB:02:ED";
    const PIXEL: &str = "10:2B:41:7C:00:9E";

    #[test]
    fn phones_are_kept_most_recent_first_and_capped() {
        let directory = tempfile::tempdir().expect("tempdir");
        let state_dir = directory.path();
        assert!(load(state_dir).is_empty());

        remember(state_dir, PhoneKind::CarPlay, IPHONE, Some("Neel's iPhone")).expect("iphone");
        remember(state_dir, PhoneKind::AndroidAuto, PIXEL, None).expect("pixel");
        let phones = load(state_dir);
        assert_eq!(phones[0].address, PIXEL);
        assert_eq!(phones[0].name, PIXEL);
        assert_eq!(phones[1].name, "Neel's iPhone");

        remember(state_dir, PhoneKind::CarPlay, IPHONE, None).expect("again");
        let phones = load(state_dir);
        assert_eq!(phones[0].address, IPHONE);
        assert_eq!(phones[0].name, "Neel's iPhone", "a known name survives");
        assert_eq!(phones.len(), 2);

        for octet in 0..MAXIMUM_KNOWN_PHONES {
            let address = format!("00:00:00:00:00:{octet:02X}");
            remember(state_dir, PhoneKind::CarPlay, &address, None).expect("fill");
        }
        assert_eq!(load(state_dir).len(), MAXIMUM_KNOWN_PHONES);
        assert!(load(state_dir).iter().all(|phone| phone.address != PIXEL));
    }

    #[test]
    fn promote_and_forget_reorder_and_remove() {
        let directory = tempfile::tempdir().expect("tempdir");
        let state_dir = directory.path();
        remember(state_dir, PhoneKind::CarPlay, IPHONE, Some("iPhone")).expect("iphone");
        remember(state_dir, PhoneKind::AndroidAuto, PIXEL, Some("Pixel")).expect("pixel");

        let promoted = promote(state_dir, IPHONE).expect("promote");
        assert_eq!(promoted.map(|phone| phone.name), Some("iPhone".to_owned()));
        assert_eq!(load(state_dir)[0].address, IPHONE);
        assert_eq!(
            promote(state_dir, "00:11:22:33:44:55").expect("missing"),
            None
        );

        forget(state_dir, IPHONE).expect("forget");
        let phones = load(state_dir);
        assert_eq!(phones.len(), 1);
        assert_eq!(phones[0].address, PIXEL);
    }

    #[test]
    fn garbage_is_rejected_and_the_legacy_file_migrates() {
        let directory = tempfile::tempdir().expect("tempdir");
        let state_dir = directory.path();
        assert!(remember(state_dir, PhoneKind::CarPlay, "../etc/passwd", None).is_err());

        fs::write(
            state_dir.join(LEGACY_LAST_PHONE_FILE),
            format!("carplay {IPHONE}"),
        )
        .expect("legacy");
        let phones = load(state_dir);
        assert_eq!(phones.len(), 1);
        assert_eq!(phones[0].kind, PhoneKind::CarPlay);

        remember(
            state_dir,
            PhoneKind::AndroidAuto,
            PIXEL,
            Some("Line\nbreak"),
        )
        .expect("pixel");
        assert!(!state_dir.join(LEGACY_LAST_PHONE_FILE).exists());
        let phones = load(state_dir);
        assert_eq!(phones.len(), 2);
        assert_eq!(phones[0].name, "Line break");

        fs::write(
            state_dir.join(PHONES_FILE),
            "carplay not-an-address\nbogus A8:AB:B5:AB:02:ED\n",
        )
        .expect("corrupt");
        assert!(load(state_dir).is_empty());
    }
}
