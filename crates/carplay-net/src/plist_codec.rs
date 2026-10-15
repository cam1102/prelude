use std::io::Cursor;

use plist::{Dictionary, Value};

use crate::{Error, Result};

const BINARY_PLIST_MAGIC: &[u8; 8] = b"bplist00";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlistLimits {
    pub maximum_encoded_bytes: usize,
    pub maximum_nodes: usize,
    pub maximum_depth: usize,
    pub maximum_string_bytes: usize,
    pub maximum_data_bytes: usize,
}

impl Default for PlistLimits {
    fn default() -> Self {
        Self {
            maximum_encoded_bytes: 1024 * 1024,
            maximum_nodes: 2048,
            maximum_depth: 24,
            maximum_string_bytes: 64 * 1024,
            maximum_data_bytes: 512 * 1024,
        }
    }
}

impl PlistLimits {
    pub(crate) fn validate(self) -> Result<Self> {
        if self.maximum_encoded_bytes < 40
            || self.maximum_nodes == 0
            || self.maximum_depth == 0
            || self.maximum_string_bytes == 0
            || self.maximum_data_bytes == 0
        {
            return Err(Error::InvalidRtsp("invalid plist limits".to_owned()));
        }
        Ok(self)
    }
}

pub(crate) fn decode_dictionary(body: &[u8], limits: PlistLimits) -> Result<Dictionary> {
    let value = decode(body, limits)?;
    match value {
        Value::Dictionary(dictionary) => Ok(dictionary),
        _ => Err(Error::InvalidRtsp(
            "binary plist root is not a dictionary".to_owned(),
        )),
    }
}

pub(crate) fn decode(body: &[u8], limits: PlistLimits) -> Result<Value> {
    let limits = limits.validate()?;
    if body.len() > limits.maximum_encoded_bytes {
        return Err(Error::MessageTooLarge {
            section: "plist",
            actual: body.len(),
            maximum: limits.maximum_encoded_bytes,
        });
    }
    if !body.starts_with(BINARY_PLIST_MAGIC) {
        return Err(Error::InvalidRtsp(
            "body is not an Apple binary plist".to_owned(),
        ));
    }
    let value = Value::from_reader(Cursor::new(body))
        .map_err(|error| Error::InvalidRtsp(format!("invalid binary plist: {error}")))?;
    let mut nodes = 0;
    audit_value(&value, 0, &mut nodes, limits)?;
    Ok(value)
}

pub(crate) fn encode(value: &Value, limits: PlistLimits) -> Result<Vec<u8>> {
    let limits = limits.validate()?;
    let mut nodes = 0;
    audit_value(value, 0, &mut nodes, limits)?;
    let mut encoded = Vec::new();
    value
        .to_writer_binary(&mut encoded)
        .map_err(|error| Error::InvalidRtsp(format!("could not encode binary plist: {error}")))?;
    if encoded.len() > limits.maximum_encoded_bytes {
        return Err(Error::MessageTooLarge {
            section: "plist",
            actual: encoded.len(),
            maximum: limits.maximum_encoded_bytes,
        });
    }
    Ok(encoded)
}

fn audit_value(value: &Value, depth: usize, nodes: &mut usize, limits: PlistLimits) -> Result<()> {
    if depth > limits.maximum_depth {
        return Err(Error::InvalidRtsp(
            "binary plist nesting is too deep".to_owned(),
        ));
    }
    *nodes = nodes
        .checked_add(1)
        .ok_or_else(|| Error::InvalidRtsp("binary plist node count overflow".to_owned()))?;
    if *nodes > limits.maximum_nodes {
        return Err(Error::InvalidRtsp(
            "binary plist has too many values".to_owned(),
        ));
    }
    match value {
        Value::Array(array) => {
            for child in array {
                audit_value(child, depth + 1, nodes, limits)?;
            }
        }
        Value::Dictionary(dictionary) => {
            for (key, child) in dictionary {
                if key.len() > limits.maximum_string_bytes {
                    return Err(Error::InvalidRtsp(
                        "binary plist dictionary key is too long".to_owned(),
                    ));
                }
                audit_value(child, depth + 1, nodes, limits)?;
            }
        }
        Value::String(string) if string.len() > limits.maximum_string_bytes => {
            return Err(Error::InvalidRtsp(
                "binary plist string is too long".to_owned(),
            ));
        }
        Value::Data(data) if data.len() > limits.maximum_data_bytes => {
            return Err(Error::InvalidRtsp(
                "binary plist data is too long".to_owned(),
            ));
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_livi_binary_plist_golden() {
        let encoded = hex::decode("62706c6973743030d60102030405060708090a0b0c546e616d655864657669636549445a6d616341646472657373556d6f64656c5a74696d696e67506f72745f10116b656570416c6976654c6f77506f776572566950686f6e655541413a42425543433a44445a6950686f6e6531352c32111b620908151a232e343f535a606671740000000000000101000000000000000d00000000000000000000000000000075").expect("hex");
        let dictionary = decode_dictionary(&encoded, PlistLimits::default()).expect("plist");
        assert_eq!(
            dictionary.get("name").and_then(Value::as_string),
            Some("iPhone")
        );
        assert_eq!(
            dictionary
                .get("timingPort")
                .and_then(Value::as_unsigned_integer),
            Some(7010)
        );
    }

    #[test]
    fn rejects_xml_oversized_and_deep_plists() {
        assert!(matches!(
            decode(b"<?xml version='1.0'?><plist/>", PlistLimits::default()),
            Err(Error::InvalidRtsp(_))
        ));
        let value = Value::Data(vec![0; 32]);
        let limits = PlistLimits {
            maximum_data_bytes: 16,
            ..PlistLimits::default()
        };
        assert!(matches!(encode(&value, limits), Err(Error::InvalidRtsp(_))));
        let value = Value::Array(vec![Value::Array(vec![Value::Boolean(true)])]);
        let limits = PlistLimits {
            maximum_depth: 1,
            ..PlistLimits::default()
        };
        assert!(matches!(encode(&value, limits), Err(Error::InvalidRtsp(_))));
    }
}
