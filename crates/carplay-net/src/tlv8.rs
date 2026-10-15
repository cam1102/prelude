use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tlv8Limits {
    pub maximum_encoded_bytes: usize,
    pub maximum_value_bytes: usize,
    pub maximum_items: usize,
}

impl Default for Tlv8Limits {
    fn default() -> Self {
        Self {
            maximum_encoded_bytes: 64 * 1024,
            maximum_value_bytes: 16 * 1024,
            maximum_items: 64,
        }
    }
}

impl Tlv8Limits {
    fn validate(self) -> Result<Self> {
        if self.maximum_encoded_bytes < 2
            || self.maximum_value_bytes == 0
            || self.maximum_items == 0
            || self.maximum_value_bytes > self.maximum_encoded_bytes
        {
            return Err(Error::InvalidTlv8("invalid codec limits".to_owned()));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tlv8Item {
    pub kind: u8,
    pub value: Vec<u8>,
}

impl Tlv8Item {
    pub fn new(kind: u8, value: impl Into<Vec<u8>>) -> Self {
        Self {
            kind,
            value: value.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tlv8 {
    items: Vec<Tlv8Item>,
}

impl Tlv8 {
    pub fn new(items: Vec<Tlv8Item>) -> Self {
        Self { items }
    }

    pub fn decode(input: &[u8], limits: Tlv8Limits) -> Result<Self> {
        let limits = limits.validate()?;
        if input.len() > limits.maximum_encoded_bytes {
            return Err(Error::InvalidTlv8(format!(
                "message is {} bytes; maximum is {}",
                input.len(),
                limits.maximum_encoded_bytes
            )));
        }

        let mut items: Vec<Tlv8Item> = Vec::new();
        let mut offset = 0usize;
        let mut previous_wire_kind = None;
        let mut previous_wire_length = 0usize;
        let mut wire_items = 0usize;
        while offset < input.len() {
            let header_end = offset
                .checked_add(2)
                .ok_or_else(|| Error::InvalidTlv8("offset overflow".to_owned()))?;
            if header_end > input.len() {
                return Err(Error::InvalidTlv8(format!(
                    "truncated item header at offset {offset}"
                )));
            }
            let kind = input[offset];
            let length = usize::from(input[offset + 1]);
            let value_end = header_end
                .checked_add(length)
                .ok_or_else(|| Error::InvalidTlv8("item length overflow".to_owned()))?;
            if value_end > input.len() {
                return Err(Error::InvalidTlv8(format!(
                    "item 0x{kind:02x} at offset {offset} declares {length} bytes with only {} available",
                    input.len() - header_end
                )));
            }
            wire_items = wire_items
                .checked_add(1)
                .ok_or_else(|| Error::InvalidTlv8("item count overflow".to_owned()))?;
            if wire_items > limits.maximum_items {
                return Err(Error::InvalidTlv8(format!(
                    "message has more than {} wire items",
                    limits.maximum_items
                )));
            }

            let value = &input[header_end..value_end];
            if previous_wire_kind == Some(kind) && previous_wire_length == usize::from(u8::MAX) {
                let item = items.last_mut().ok_or_else(|| {
                    Error::InvalidTlv8("fragment has no preceding item".to_owned())
                })?;
                let combined = item.value.len().checked_add(value.len()).ok_or_else(|| {
                    Error::InvalidTlv8("fragmented value length overflow".to_owned())
                })?;
                if combined > limits.maximum_value_bytes {
                    return Err(Error::InvalidTlv8(format!(
                        "value 0x{kind:02x} is {combined} bytes; maximum is {}",
                        limits.maximum_value_bytes
                    )));
                }
                item.value.extend_from_slice(value);
            } else {
                if value.len() > limits.maximum_value_bytes {
                    return Err(Error::InvalidTlv8(format!(
                        "value 0x{kind:02x} is {} bytes; maximum is {}",
                        value.len(),
                        limits.maximum_value_bytes
                    )));
                }
                items.push(Tlv8Item::new(kind, value.to_vec()));
            }
            previous_wire_kind = Some(kind);
            previous_wire_length = length;
            offset = value_end;
        }
        Ok(Self { items })
    }

    pub fn encode(&self, limits: Tlv8Limits) -> Result<Vec<u8>> {
        let limits = limits.validate()?;
        if self.items.len() > limits.maximum_items {
            return Err(Error::InvalidTlv8(format!(
                "message has {} logical items; maximum is {}",
                self.items.len(),
                limits.maximum_items
            )));
        }
        let mut encoded = Vec::new();
        let mut previous_kind = None;
        for item in &self.items {
            if item.value.len() > limits.maximum_value_bytes {
                return Err(Error::InvalidTlv8(format!(
                    "value 0x{:02x} is {} bytes; maximum is {}",
                    item.kind,
                    item.value.len(),
                    limits.maximum_value_bytes
                )));
            }
            if previous_kind == Some(item.kind) {
                append_wire_item(&mut encoded, TlvType::SEPARATOR, &[], limits)?;
            }
            if item.value.is_empty() {
                append_wire_item(&mut encoded, item.kind, &[], limits)?;
            } else {
                for fragment in item.value.chunks(usize::from(u8::MAX)) {
                    append_wire_item(&mut encoded, item.kind, fragment, limits)?;
                }
            }
            previous_kind = Some(item.kind);
        }
        Ok(encoded)
    }

    pub fn items(&self) -> &[Tlv8Item] {
        &self.items
    }

    pub fn optional_unique(&self, kind: u8) -> Result<Option<&[u8]>> {
        let mut matches = self.items.iter().filter(|item| item.kind == kind);
        let first = matches.next().map(|item| item.value.as_slice());
        if matches.next().is_some() {
            return Err(Error::InvalidTlv8(format!("duplicate field 0x{kind:02x}")));
        }
        Ok(first)
    }

    pub fn required_unique(&self, kind: u8) -> Result<&[u8]> {
        self.optional_unique(kind)?
            .ok_or_else(|| Error::InvalidTlv8(format!("missing field 0x{kind:02x}")))
    }

    pub fn require_only(&self, allowed: &[u8]) -> Result<()> {
        if let Some(item) = self
            .items
            .iter()
            .find(|item| item.kind != TlvType::SEPARATOR && !allowed.contains(&item.kind))
        {
            return Err(Error::InvalidTlv8(format!(
                "unexpected field 0x{:02x}",
                item.kind
            )));
        }
        Ok(())
    }
}

fn append_wire_item(
    output: &mut Vec<u8>,
    kind: u8,
    value: &[u8],
    limits: Tlv8Limits,
) -> Result<()> {
    let new_length = output
        .len()
        .checked_add(2)
        .and_then(|length| length.checked_add(value.len()))
        .ok_or_else(|| Error::InvalidTlv8("encoded length overflow".to_owned()))?;
    if new_length > limits.maximum_encoded_bytes {
        return Err(Error::InvalidTlv8(format!(
            "encoded message is {new_length} bytes; maximum is {}",
            limits.maximum_encoded_bytes
        )));
    }
    output.push(kind);
    output.push(
        u8::try_from(value.len())
            .map_err(|_| Error::InvalidTlv8("fragment exceeds 255 bytes".to_owned()))?,
    );
    output.extend_from_slice(value);
    Ok(())
}

#[derive(Debug)]
pub struct TlvType;

impl TlvType {
    pub const METHOD: u8 = 0x00;
    pub const IDENTIFIER: u8 = 0x01;
    pub const SALT: u8 = 0x02;
    pub const PUBLIC_KEY: u8 = 0x03;
    pub const PROOF: u8 = 0x04;
    pub const ENCRYPTED_DATA: u8 = 0x05;
    pub const STATE: u8 = 0x06;
    pub const ERROR: u8 = 0x07;
    pub const SIGNATURE: u8 = 0x0a;
    pub const SEPARATOR: u8 = 0xff;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn livi_golden_short_empty_fragmented_and_separated_items() {
        let limits = Tlv8Limits::default();
        assert_eq!(
            Tlv8::new(vec![Tlv8Item::new(TlvType::STATE, [1])])
                .encode(limits)
                .expect("state"),
            [0x06, 0x01, 0x01]
        );
        assert_eq!(
            Tlv8::new(vec![Tlv8Item::new(0x0b, [])])
                .encode(limits)
                .expect("empty"),
            [0x0b, 0x00]
        );

        let fragmented = Tlv8::new(vec![Tlv8Item::new(0x05, vec![0xab; 300])])
            .encode(limits)
            .expect("fragmented");
        assert_eq!(&fragmented[..2], &[0x05, 0xff]);
        assert_eq!(&fragmented[257..259], &[0x05, 45]);
        assert_eq!(
            Tlv8::decode(&fragmented, limits)
                .expect("decode")
                .required_unique(0x05)
                .expect("field"),
            vec![0xab; 300]
        );

        assert_eq!(
            Tlv8::new(vec![Tlv8Item::new(1, [0xaa]), Tlv8Item::new(1, [0xbb]),])
                .encode(limits)
                .expect("separated"),
            [1, 1, 0xaa, 0xff, 0, 1, 1, 0xbb]
        );
    }

    #[test]
    fn truncated_or_oversized_input_is_rejected() {
        let limits = Tlv8Limits {
            maximum_encoded_bytes: 8,
            maximum_value_bytes: 4,
            maximum_items: 2,
        };
        assert!(matches!(
            Tlv8::decode(&[1], limits),
            Err(Error::InvalidTlv8(_))
        ));
        assert!(matches!(
            Tlv8::decode(&[1, 2, 1], limits),
            Err(Error::InvalidTlv8(_))
        ));
        assert!(matches!(
            Tlv8::decode(&[1, 5, 0, 0, 0, 0, 0], limits),
            Err(Error::InvalidTlv8(_))
        ));
    }
}
