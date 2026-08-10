use pintail_types::{Float64, KeyPart, PrimaryKey, StoredRow, Value};

use crate::StoreError;

pub(crate) struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    pub(crate) fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    pub(crate) fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn length(&mut self, value: usize, what: &str) -> Result<(), StoreError> {
        let value = u32::try_from(value)
            .map_err(|_| StoreError::FormatLimit(format!("{what} exceeds u32::MAX")))?;
        self.u32(value);
        Ok(())
    }

    pub(crate) fn bytes(&mut self, value: &[u8], what: &str) -> Result<(), StoreError> {
        self.length(value.len(), what)?;
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    pub(crate) fn raw(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

pub(crate) struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
    base_offset: usize,
}

impl<'a> Decoder<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            position: 0,
            base_offset: 0,
        }
    }

    pub(crate) fn with_base_offset(bytes: &'a [u8], base_offset: usize) -> Self {
        Self {
            bytes,
            position: 0,
            base_offset,
        }
    }

    pub(crate) fn u8(&mut self) -> Result<u8, String> {
        let byte = *self
            .bytes
            .get(self.position)
            .ok_or_else(|| "unexpected end while reading u8".to_owned())?;
        self.position += 1;
        Ok(byte)
    }

    pub(crate) fn u32(&mut self) -> Result<u32, String> {
        let bytes = self.take_array::<4>()?;
        Ok(u32::from_le_bytes(bytes))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, String> {
        let bytes = self.take_array::<8>()?;
        Ok(u64::from_le_bytes(bytes))
    }

    pub(crate) fn i64(&mut self) -> Result<i64, String> {
        let bytes = self.take_array::<8>()?;
        Ok(i64::from_le_bytes(bytes))
    }

    pub(crate) fn bytes(&mut self) -> Result<&'a [u8], String> {
        let length =
            usize::try_from(self.u32()?).map_err(|_| "length does not fit usize".to_owned())?;
        self.take(length)
    }

    pub(crate) fn finish(self) -> Result<(), String> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(format!(
                "{} trailing bytes after record payload",
                self.bytes.len() - self.position
            ))
        }
    }

    pub(crate) fn position(&self) -> usize {
        self.base_offset.saturating_add(self.position)
    }

    pub(crate) fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| "length overflow".to_owned())?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| format!("unexpected end while reading {length} bytes"))?;
        self.position = end;
        Ok(bytes)
    }

    fn take_array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        self.take(N)?
            .try_into()
            .map_err(|_| format!("expected {N} bytes"))
    }
}

pub(crate) fn encode_key(encoder: &mut Encoder, key: &PrimaryKey) -> Result<(), StoreError> {
    encoder.length(key.parts().len(), "primary-key component count")?;
    for part in key.parts() {
        match part {
            KeyPart::Int64(value) => {
                encoder.u8(0);
                encoder.i64(*value);
            }
            KeyPart::UInt64(value) => {
                encoder.u8(1);
                encoder.u64(*value);
            }
            KeyPart::Utf8(value) => {
                encoder.u8(2);
                encoder.bytes(value.as_bytes(), "UTF-8 key")?;
            }
            KeyPart::Binary(value) => {
                encoder.u8(3);
                encoder.bytes(value, "binary key")?;
            }
        }
    }
    Ok(())
}

pub(crate) fn decode_key(decoder: &mut Decoder<'_>) -> Result<PrimaryKey, String> {
    let key_count = decoder.u32()?;
    let mut key = Vec::with_capacity(key_count as usize);
    for _ in 0..key_count {
        key.push(match decoder.u8()? {
            0 => KeyPart::Int64(decoder.i64()?),
            1 => KeyPart::UInt64(decoder.u64()?),
            2 => KeyPart::Utf8(decode_utf8(decoder.bytes()?, "UTF-8 key")?),
            3 => KeyPart::Binary(decoder.bytes()?.to_vec()),
            tag => return Err(format!("unknown key type tag {tag}")),
        });
    }
    PrimaryKey::new(key).map_err(|error| error.to_string())
}

pub(crate) fn encode_row(encoder: &mut Encoder, row: &StoredRow) -> Result<(), StoreError> {
    encode_key(encoder, row.key())?;

    encoder.length(row.values().len(), "row value count")?;
    for value in row.values() {
        encode_value(encoder, value)?;
    }
    encoder.u64(row.version());
    encoder.u8(u8::from(row.is_deleted()));
    Ok(())
}

pub(crate) fn decode_row(decoder: &mut Decoder<'_>) -> Result<StoredRow, String> {
    let key = decode_key(decoder)?;

    let value_count = decoder.u32()?;
    let mut values = Vec::with_capacity(value_count as usize);
    for _ in 0..value_count {
        values.push(decode_value(decoder)?);
    }
    let version = decoder.u64()?;
    let deleted = match decoder.u8()? {
        0 => false,
        1 => true,
        value => return Err(format!("invalid tombstone flag {value}")),
    };
    Ok(StoredRow::new(key, values, version, deleted))
}

fn encode_value(encoder: &mut Encoder, value: &Value) -> Result<(), StoreError> {
    match value {
        Value::Null => encoder.u8(0),
        Value::Boolean(value) => {
            encoder.u8(1);
            encoder.u8(u8::from(*value));
        }
        Value::Int64(value) => {
            encoder.u8(2);
            encoder.i64(*value);
        }
        Value::UInt64(value) => {
            encoder.u8(3);
            encoder.u64(*value);
        }
        Value::Float64(value) => {
            encoder.u8(4);
            encoder.u64(value.to_bits());
        }
        Value::Utf8(value) => {
            encoder.u8(5);
            encoder.bytes(value.as_bytes(), "UTF-8 value")?;
        }
        Value::Binary(value) => {
            encoder.u8(6);
            encoder.bytes(value, "binary value")?;
        }
        // The label is the durable form. An ENUM's index is a property of
        // the column declaration, not of the value, so persisting it per row
        // would store the same number a million times and go stale the
        // moment the declaration changes.
        Value::Enum { label, .. } => {
            encoder.u8(5);
            encoder.bytes(label.as_bytes(), "UTF-8 value")?;
        }
    }
    Ok(())
}

fn decode_value(decoder: &mut Decoder<'_>) -> Result<Value, String> {
    match decoder.u8()? {
        0 => Ok(Value::Null),
        1 => match decoder.u8()? {
            0 => Ok(Value::Boolean(false)),
            1 => Ok(Value::Boolean(true)),
            value => Err(format!("invalid boolean value {value}")),
        },
        2 => Ok(Value::Int64(decoder.i64()?)),
        3 => Ok(Value::UInt64(decoder.u64()?)),
        4 => Ok(Value::Float64(Float64::new(f64::from_bits(decoder.u64()?)))),
        5 => Ok(Value::Utf8(decode_utf8(decoder.bytes()?, "UTF-8 value")?)),
        6 => Ok(Value::Binary(decoder.bytes()?.to_vec())),
        tag => Err(format!("unknown value type tag {tag}")),
    }
}

fn decode_utf8(bytes: &[u8], what: &str) -> Result<String, String> {
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|error| format!("invalid {what}: {error}"))
}
