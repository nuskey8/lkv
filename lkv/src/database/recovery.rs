use super::state::{BaseBytes, KeyMap, MAPPED_VALUE_THRESHOLD, MappedMutation, OverlayMap};
use crate::format::log::{
    LOG_HEADER_CHECKSUM_OFFSET, LOG_HEADER_SIZE, MAX_BATCH_OPERATIONS, MAX_LOG_PAYLOAD_SIZE,
    Marker, record_checksum, record_checksum_start,
};
use crate::format::segment::{MAX_KEY_SIZE, MAX_VALUE_SIZE};
use crate::{Error, Result};
use crc32c::crc32c_append;
use std::io::{BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom};

const RECOVERY_BUFFER_SIZE: usize = 64 * 1024;

pub struct OverlayScan {
    index: KeyMap<RecoveredOverlayEntry>,
    valid_len: u64,
}

impl OverlayScan {
    pub fn valid_len(&self) -> u64 {
        self.valid_len
    }

    pub fn into_index(
        self,
        mapping: Option<BaseBytes>,
        mapping_offset: u64,
    ) -> Result<(OverlayMap, usize)> {
        let mut index = OverlayMap::mapped(mapping);
        index
            .try_reserve_mapped(self.index.len())
            .map_err(|_| Error::invalid_base("Overlay index allocation failed"))?;
        let mut memory = 0usize;
        for (key, entry) in self.index {
            let operation_offset = entry
                .operation_offset
                .checked_sub(mapping_offset)
                .and_then(|offset| usize::try_from(offset).ok())
                .ok_or_else(|| Error::invalid_base("Overlay operation offset overflow"))?;
            if operation_offset
                .checked_add(9)
                .is_none_or(|end| end > index.mapped_bytes_len())
            {
                return Err(Error::invalid_base(
                    "Overlay operation lies outside its mapping",
                ));
            }
            let mutation = MappedMutation { operation_offset };
            memory = memory.saturating_add(key.len() + entry.value_memory);
            index.install_recovered(mutation, xxhash_rust::xxh3::xxh3_64(&key));
        }
        Ok((index, memory))
    }
}

struct RecoveredOverlayEntry {
    operation_offset: u64,
    value_memory: usize,
}

pub fn scan_overlay(file: &mut (impl Read + Seek), start: u64, end: u64) -> Result<OverlayScan> {
    if start > end {
        return Err(Error::invalid_base("overlay range precedes active Base"));
    }
    let mut offset = start;
    let mut index = KeyMap::default();
    if start == end {
        return Ok(OverlayScan {
            index,
            valid_len: start,
        });
    }
    let mut key_buffer = Vec::new();
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::with_capacity(RECOVERY_BUFFER_SIZE, file);
    while offset < end {
        let mut header = [0; LOG_HEADER_SIZE];
        match reader.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error.into()),
        }
        let expected_header_checksum = u32::from_le_bytes(
            header[LOG_HEADER_CHECKSUM_OFFSET..LOG_HEADER_SIZE]
                .try_into()
                .unwrap(),
        );
        if crc32c::crc32c(&header[..LOG_HEADER_CHECKSUM_OFFSET]) != expected_header_checksum {
            return Err(Error::corrupted_log(
                offset,
                format!("overlay header checksum mismatch at offset {offset}"),
            ));
        }
        let marker_byte = header[0];
        let key_len = u32::from_le_bytes(header[1..5].try_into().unwrap()) as usize;
        let value_len = u32::from_le_bytes(header[5..9].try_into().unwrap()) as usize;
        let expected_record_checksum = u32::from_le_bytes(header[9..13].try_into().unwrap());
        let record_len = LOG_HEADER_SIZE as u64 + key_len as u64 + value_len as u64;
        if offset
            .checked_add(record_len)
            .is_none_or(|record_end| record_end > end)
        {
            break;
        }
        let Some(marker) = Marker::from_byte(marker_byte) else {
            return Err(Error::from_io(
                ErrorKind::InvalidData,
                format!("invalid overlay operation at offset {offset}"),
            ));
        };
        if marker == Marker::Compact && key_len == 0 && value_len == 0 {
            if expected_record_checksum == record_checksum(marker, 0, 0, &[]) {
                break;
            }
            return Err(Error::corrupted_log(
                offset,
                format!("invalid compact marker checksum at offset {offset}"),
            ));
        }
        if marker != Marker::Batch || key_len != 0 {
            return Err(Error::from_io(
                ErrorKind::InvalidData,
                format!("unexpected overlay marker at offset {offset}"),
            ));
        }
        let payload_len = key_len.checked_add(value_len).ok_or_else(|| {
            Error::from_io(ErrorKind::InvalidData, "overlay record length overflow")
        })?;
        if payload_len > MAX_LOG_PAYLOAD_SIZE {
            return Err(Error::from_io(
                ErrorKind::InvalidData,
                "overlay record exceeds configured format limits",
            ));
        }
        let payload_offset = offset + LOG_HEADER_SIZE as u64;
        let mut recovery = RecoveryState {
            index: &mut index,
            key_buffer: &mut key_buffer,
        };
        let structure = recover_batch_payload(
            &mut reader,
            payload_offset,
            payload_len,
            record_checksum_start(marker, key_len as u32, value_len as u32),
            &mut recovery,
        )?;
        if structure.checksum != expected_record_checksum {
            return Err(Error::corrupted_log(
                offset,
                format!("overlay checksum mismatch at offset {offset}"),
            ));
        }
        if let Some(error) = structure.error {
            return Err(error);
        }
        offset += record_len;
    }
    Ok(OverlayScan {
        index,
        valid_len: offset,
    })
}

struct BatchValidation {
    checksum: u32,
    error: Option<Error>,
}

struct RecoveryState<'a> {
    index: &'a mut KeyMap<RecoveredOverlayEntry>,
    key_buffer: &'a mut Vec<u8>,
}

fn recover_batch_payload<R: BufRead>(
    reader: &mut R,
    payload_offset: u64,
    payload_len: usize,
    checksum: u32,
    recovery: &mut RecoveryState<'_>,
) -> Result<BatchValidation> {
    let mut batch = BatchReader::checksummed(reader, payload_len, checksum);
    let error = apply_batch_payload(&mut batch, payload_offset, recovery).err();
    let checksum = batch.finish_checksum()?;
    Ok(BatchValidation { checksum, error })
}

fn apply_batch_payload<R: BufRead>(
    reader: &mut BatchReader<R>,
    payload_offset: u64,
    recovery: &mut RecoveryState<'_>,
) -> Result<()> {
    let count = reader.read_count()?;
    if recovery.index.is_empty() {
        recovery
            .index
            .try_reserve(count)
            .map_err(|_| Error::invalid_base("Overlay index allocation failed"))?;
    }
    for _ in 0..count {
        let operation_offset = payload_offset
            .checked_add(
                u64::try_from(reader.consumed())
                    .map_err(|_| Error::invalid_base("overlay operation offset overflow"))?,
            )
            .ok_or_else(|| Error::invalid_base("overlay operation offset overflow"))?;
        let operation = reader.read_operation()?;
        if operation.key_len > recovery.key_buffer.capacity() {
            recovery
                .key_buffer
                .try_reserve_exact(operation.key_len - recovery.key_buffer.len())
                .map_err(|_| Error::invalid_base("batch key allocation failed"))?;
        }
        recovery.key_buffer.resize(operation.key_len, 0);
        reader.read_exact(recovery.key_buffer, "truncated batch key")?;
        reader.skip(operation.value_len)?;
        let entry = RecoveredOverlayEntry {
            operation_offset,
            value_memory: operation.value_memory,
        };
        if let Some(previous) = recovery.index.get_mut(recovery.key_buffer.as_slice()) {
            *previous = entry;
        } else {
            let key = std::mem::take(recovery.key_buffer);
            recovery
                .index
                .try_reserve(1)
                .map_err(|_| Error::invalid_base("batch key allocation failed"))?;
            recovery.index.insert(key, entry);
        }
    }
    reader.require_end()
}

struct BatchOperation {
    key_len: usize,
    value_len: usize,
    value_memory: usize,
}

struct BatchReader<R> {
    inner: R,
    payload_len: usize,
    remaining: usize,
    checksum: Option<u32>,
}

impl<R: BufRead> BatchReader<R> {
    fn checksummed(inner: R, payload_len: usize, checksum: u32) -> Self {
        Self {
            inner,
            payload_len,
            remaining: payload_len,
            checksum: Some(checksum),
        }
    }

    fn read_count(&mut self) -> Result<usize> {
        let mut bytes = [0; 4];
        self.read_exact(&mut bytes, "truncated batch count")?;
        let count = u32::from_le_bytes(bytes) as usize;
        if count > MAX_BATCH_OPERATIONS {
            return Err(Error::from_io(
                ErrorKind::InvalidData,
                "batch operation count exceeds the runtime limit",
            ));
        }
        Ok(count)
    }

    fn read_operation(&mut self) -> Result<BatchOperation> {
        let mut header = [0; 9];
        self.read_exact(&mut header, "truncated batch operation")?;
        let marker = Marker::from_byte(header[0]);
        let key_len = u32::from_le_bytes(header[1..5].try_into().unwrap()) as usize;
        let value_len = u32::from_le_bytes(header[5..9].try_into().unwrap()) as usize;
        if !matches!(marker, Some(Marker::Put | Marker::Delete))
            || (marker == Some(Marker::Delete) && value_len != 0)
            || key_len > MAX_KEY_SIZE
            || value_len > MAX_VALUE_SIZE
        {
            return Err(Error::from_io(
                ErrorKind::InvalidData,
                "invalid batch operation",
            ));
        }
        let data_len = key_len
            .checked_add(value_len)
            .filter(|length| *length <= self.remaining)
            .ok_or_else(|| Error::from_io(ErrorKind::InvalidData, "truncated batch value"))?;
        debug_assert!(data_len <= self.remaining);
        Ok(BatchOperation {
            key_len,
            value_len,
            value_memory: if marker == Some(Marker::Put) && value_len < MAPPED_VALUE_THRESHOLD {
                value_len
            } else {
                0
            },
        })
    }

    fn consumed(&self) -> usize {
        self.payload_len - self.remaining
    }

    fn skip(&mut self, mut len: usize) -> Result<()> {
        if len > self.remaining {
            return Err(Error::from_io(
                ErrorKind::InvalidData,
                "truncated batch data",
            ));
        }
        while len > 0 {
            let buffer = self.inner.fill_buf()?;
            if buffer.is_empty() {
                return Err(Error::from_io(
                    ErrorKind::UnexpectedEof,
                    "truncated batch data",
                ));
            }
            let chunk = len.min(buffer.len());
            if let Some(checksum) = &mut self.checksum {
                *checksum = crc32c_append(*checksum, &buffer[..chunk]);
            }
            self.inner.consume(chunk);
            self.remaining -= chunk;
            len -= chunk;
        }
        Ok(())
    }

    fn require_end(&self) -> Result<()> {
        if self.remaining == 0 {
            Ok(())
        } else {
            Err(Error::from_io(
                ErrorKind::InvalidData,
                "trailing bytes in batch",
            ))
        }
    }

    fn finish_checksum(mut self) -> Result<u32> {
        let remaining = self.remaining;
        self.skip(remaining)?;
        Ok(self
            .checksum
            .expect("checksummed reader must have a checksum"))
    }

    fn read_exact(&mut self, bytes: &mut [u8], truncated: &'static str) -> Result<()> {
        if bytes.len() > self.remaining {
            return Err(Error::from_io(ErrorKind::InvalidData, truncated));
        }
        self.inner.read_exact(bytes)?;
        self.remaining -= bytes.len();
        if let Some(checksum) = &mut self.checksum {
            *checksum = crc32c_append(*checksum, bytes);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    struct HeaderReadFailure;

    impl Read for HeaderReadFailure {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "injected header read failure",
            ))
        }
    }

    impl Seek for HeaderReadFailure {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            match position {
                SeekFrom::Start(offset) => Ok(offset),
                _ => unreachable!("overlay recovery seeks from the start"),
            }
        }
    }

    #[test]
    fn non_eof_header_read_errors_are_propagated() {
        let error = scan_overlay(&mut HeaderReadFailure, 0, LOG_HEADER_SIZE as u64)
            .err()
            .expect("non-EOF read failure must not be treated as a torn tail");
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    }
}
