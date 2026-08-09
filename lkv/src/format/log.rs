use crate::database::{KeyMap, OverlayEntry};
use crate::{Error, Result};
use crc32c::{crc32c, crc32c_append, crc32c_combine};
use std::io::{ErrorKind, Write};

pub const MAX_LOG_PAYLOAD_SIZE: usize = u32::MAX as usize;
pub(crate) const MAX_BATCH_OPERATIONS: usize = 1_000_000;
pub(crate) const LOG_HEADER_SIZE: usize = 17;
pub(crate) const LOG_HEADER_CHECKSUM_OFFSET: usize = 13;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Marker {
    Put = 1,
    Delete = 2,
    Compact = 3,
    Batch = 4,
}

impl Marker {
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Put),
            2 => Some(Self::Delete),
            3 => Some(Self::Compact),
            4 => Some(Self::Batch),
            _ => None,
        }
    }
}

pub fn batch_payload_len(staged: &KeyMap<OverlayEntry>) -> Result<u32> {
    if staged.len() > MAX_BATCH_OPERATIONS {
        return Err(Error::from_io(
            ErrorKind::InvalidInput,
            format!("transaction exceeds the {MAX_BATCH_OPERATIONS} operation limit"),
        ));
    }
    let mut len = 4usize;
    for (key, entry) in staged {
        let value_len = match entry {
            OverlayEntry::Put(value) => value.len(),
            OverlayEntry::Delete => 0,
        };
        len = len
            .checked_add(9)
            .and_then(|len| len.checked_add(key.len()))
            .and_then(|len| len.checked_add(value_len))
            .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "batch is too large"))?;
    }
    if len > MAX_LOG_PAYLOAD_SIZE {
        return Err(Error::from_io(
            ErrorKind::InvalidInput,
            "transaction payload exceeds the log record limit",
        ));
    }
    u32::try_from(len).map_err(|_| Error::from_io(ErrorKind::InvalidInput, "batch is too large"))
}

fn batch_record_checksum(
    staged: &KeyMap<OverlayEntry>,
    value_checksums: Option<&KeyMap<u32>>,
    payload_len: u32,
) -> Result<u32> {
    let count = u32::try_from(staged.len())
        .map_err(|_| Error::from_io(ErrorKind::InvalidInput, "too many batch operations"))?;
    let mut checksum = checksum_parts(Marker::Batch, 0, payload_len, &[], &[]);
    checksum = crc32c_append(checksum, &count.to_le_bytes());
    for (key, entry) in staged {
        let (marker, value): (Marker, &[u8]) = match entry {
            OverlayEntry::Put(value) => (Marker::Put, value.as_slice()),
            OverlayEntry::Delete => (Marker::Delete, &[]),
        };
        checksum = crc32c_append(checksum, &[marker as u8]);
        checksum = crc32c_append(checksum, &(key.len() as u32).to_le_bytes());
        checksum = crc32c_append(checksum, &(value.len() as u32).to_le_bytes());
        checksum = crc32c_append(checksum, key);
        checksum = match entry {
            OverlayEntry::Put(value) => match value_checksums.and_then(|values| values.get(key)) {
                Some(value_checksum) => crc32c_combine(checksum, *value_checksum, value.len()),
                None => crc32c_append(checksum, value.as_slice()),
            },
            OverlayEntry::Delete => checksum,
        };
    }
    Ok(checksum)
}

#[cfg(test)]
pub fn write_batch_record(writer: &mut impl Write, staged: &KeyMap<OverlayEntry>) -> Result<()> {
    write_batch_record_inner(writer, staged, None)
}

pub(crate) fn write_batch_record_with_checksums(
    writer: &mut impl Write,
    staged: &KeyMap<OverlayEntry>,
    value_checksums: &KeyMap<u32>,
) -> Result<()> {
    let value_checksums = (!value_checksums.is_empty()).then_some(value_checksums);
    write_batch_record_inner(writer, staged, value_checksums)
}

fn write_batch_record_inner(
    writer: &mut impl Write,
    staged: &KeyMap<OverlayEntry>,
    value_checksums: Option<&KeyMap<u32>>,
) -> Result<()> {
    let payload_len = batch_payload_len(staged)?;
    let count = u32::try_from(staged.len())
        .map_err(|_| Error::from_io(ErrorKind::InvalidInput, "too many batch operations"))?;
    let checksum = batch_record_checksum(staged, value_checksums, payload_len)?;
    write_record_header(writer, Marker::Batch, 0, payload_len, checksum)?;
    writer.write_all(&count.to_le_bytes())?;
    for (key, entry) in staged {
        let (marker, value): (Marker, &[u8]) = match entry {
            OverlayEntry::Put(value) => (Marker::Put, value.as_slice()),
            OverlayEntry::Delete => (Marker::Delete, &[]),
        };
        writer.write_all(&[marker as u8])?;
        writer.write_all(&(key.len() as u32).to_le_bytes())?;
        writer.write_all(&(value.len() as u32).to_le_bytes())?;
        writer.write_all(key)?;
        writer.write_all(value)?;
    }
    Ok(())
}

pub fn write_compact_marker(file: &mut impl Write) -> Result<()> {
    let checksum = checksum_parts(Marker::Compact, 0, 0, &[], &[]);
    write_record_header(file, Marker::Compact, 0, 0, checksum)
}

pub(crate) fn write_record_header(
    writer: &mut impl Write,
    marker: Marker,
    key_len: u32,
    value_len: u32,
    record_checksum: u32,
) -> Result<()> {
    let mut header = [0; LOG_HEADER_SIZE];
    header[0] = marker as u8;
    header[1..5].copy_from_slice(&key_len.to_le_bytes());
    header[5..9].copy_from_slice(&value_len.to_le_bytes());
    header[9..13].copy_from_slice(&record_checksum.to_le_bytes());
    let header_checksum = crc32c(&header[..LOG_HEADER_CHECKSUM_OFFSET]);
    header[LOG_HEADER_CHECKSUM_OFFSET..].copy_from_slice(&header_checksum.to_le_bytes());
    Ok(writer.write_all(&header)?)
}

pub fn record_checksum(marker: Marker, key_len: u32, value_len: u32, payload: &[u8]) -> u32 {
    crc32c_append(record_checksum_start(marker, key_len, value_len), payload)
}

pub(crate) fn record_checksum_start(marker: Marker, key_len: u32, value_len: u32) -> u32 {
    checksum_parts(marker, key_len, value_len, &[], &[])
}

fn checksum_parts(
    marker: Marker,
    key_len: u32,
    value_len: u32,
    first: &[u8],
    second: &[u8],
) -> u32 {
    let mut checksum = crc32c_append(0, &[marker as u8]);
    checksum = crc32c_append(checksum, &key_len.to_le_bytes());
    checksum = crc32c_append(checksum, &value_len.to_le_bytes());
    checksum = crc32c_append(checksum, first);
    crc32c_append(checksum, second)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::ValueBytes;

    #[test]
    fn record_header_bytes_are_stable() {
        let payload = 0u32.to_le_bytes();
        let record_checksum = record_checksum(Marker::Batch, 0, payload.len() as u32, &payload);
        let mut header = Vec::new();
        write_record_header(
            &mut header,
            Marker::Batch,
            0,
            payload.len() as u32,
            record_checksum,
        )
        .unwrap();
        assert_eq!(
            header,
            [
                0x04, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x1d, 0xc6, 0x0e, 0x5e, 0x7e,
                0x79, 0x3c, 0x73,
            ]
        );
    }

    #[test]
    fn cached_value_checksum_preserves_batch_bytes() {
        let value = b"a value large enough to exercise checksum combination";
        let mut staged = KeyMap::default();
        staged.insert(
            b"key".to_vec(),
            OverlayEntry::Put(ValueBytes::Owned(value.to_vec())),
        );
        let mut checksums = KeyMap::default();
        checksums.insert(b"key".to_vec(), crc32c(value));
        let mut cached_bytes = Vec::new();
        let mut uncached_bytes = Vec::new();
        write_batch_record_with_checksums(&mut cached_bytes, &staged, &checksums).unwrap();
        write_batch_record(&mut uncached_bytes, &staged).unwrap();
        assert_eq!(cached_bytes, uncached_bytes);
    }
}
