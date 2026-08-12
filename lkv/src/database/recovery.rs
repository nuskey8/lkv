use super::state::{BaseBytes, MappedMutation, OverlayMap};
use crate::format::log::{
    LOG_HEADER_CHECKSUM_OFFSET, LOG_HEADER_SIZE, MAX_BATCH_OPERATIONS, MAX_LOG_PAYLOAD_SIZE,
    Marker, record_checksum, record_checksum_start,
};
use crate::format::segment::{MAX_KEY_SIZE, MAX_VALUE_SIZE};
use crate::{Error, Result};
use crc32c::crc32c_append;
use std::io::ErrorKind;
use xxhash_rust::xxh3::xxh3_64;

pub struct OverlayScan {
    index: OverlayMap,
    memory: usize,
    valid_len: u64,
}

impl OverlayScan {
    pub fn empty(valid_len: u64) -> Self {
        Self {
            index: OverlayMap::mapped(None),
            memory: 0,
            valid_len,
        }
    }

    pub fn valid_len(&self) -> u64 {
        self.valid_len
    }

    pub fn release_mapping(&mut self) {
        self.index.replace_mapped_bytes(None);
    }

    pub fn replace_mapping(&mut self, mapping: Option<BaseBytes>) {
        self.index.replace_mapped_bytes(mapping);
    }

    pub fn into_index(self) -> (OverlayMap, usize) {
        (self.index, self.memory)
    }
}

pub fn scan_overlay(mapping: BaseBytes, mapping_offset: u64) -> Result<OverlayScan> {
    let bytes: &[u8] = &mapping;
    let mut index = OverlayMap::mapped(Some(mapping.clone()));
    let mut memory = 0usize;
    let mut offset = 0usize;

    while offset < bytes.len() {
        let Some(header_end) = offset.checked_add(LOG_HEADER_SIZE) else {
            return Err(Error::invalid_base("overlay header offset overflow"));
        };
        let Some(header) = bytes.get(offset..header_end) else {
            break;
        };
        let absolute_offset = mapping_offset
            .checked_add(offset as u64)
            .ok_or_else(|| Error::invalid_base("overlay offset overflow"))?;
        let expected_header_checksum = u32::from_le_bytes(
            header[LOG_HEADER_CHECKSUM_OFFSET..LOG_HEADER_SIZE]
                .try_into()
                .unwrap(),
        );
        if crc32c::crc32c(&header[..LOG_HEADER_CHECKSUM_OFFSET]) != expected_header_checksum {
            return Err(Error::corrupted_log(
                absolute_offset,
                format!("overlay header checksum mismatch at offset {absolute_offset}"),
            ));
        }

        let marker_byte = header[0];
        let key_len = u32::from_le_bytes(header[1..5].try_into().unwrap()) as usize;
        let value_len = u32::from_le_bytes(header[5..9].try_into().unwrap()) as usize;
        let expected_record_checksum = u32::from_le_bytes(header[9..13].try_into().unwrap());
        let payload_len = key_len.checked_add(value_len).ok_or_else(|| {
            Error::from_io(ErrorKind::InvalidData, "overlay record length overflow")
        })?;
        let Some(record_end) = header_end.checked_add(payload_len) else {
            return Err(Error::invalid_base("overlay record offset overflow"));
        };
        let Some(payload) = bytes.get(header_end..record_end) else {
            break;
        };
        let Some(marker) = Marker::from_byte(marker_byte) else {
            return Err(Error::from_io(
                ErrorKind::InvalidData,
                format!("invalid overlay operation at offset {absolute_offset}"),
            ));
        };

        if marker == Marker::Compact && key_len == 0 && value_len == 0 {
            if expected_record_checksum == record_checksum(marker, 0, 0, &[]) {
                break;
            }
            return Err(Error::corrupted_log(
                absolute_offset,
                format!("invalid compact marker checksum at offset {absolute_offset}"),
            ));
        }
        if marker != Marker::Batch || key_len != 0 {
            return Err(Error::from_io(
                ErrorKind::InvalidData,
                format!("unexpected overlay marker at offset {absolute_offset}"),
            ));
        }
        if payload_len > MAX_LOG_PAYLOAD_SIZE {
            return Err(Error::from_io(
                ErrorKind::InvalidData,
                "overlay record exceeds configured format limits",
            ));
        }

        let checksum = crc32c_append(
            record_checksum_start(marker, key_len as u32, value_len as u32),
            payload,
        );
        if checksum != expected_record_checksum {
            return Err(Error::corrupted_log(
                absolute_offset,
                format!("overlay checksum mismatch at offset {absolute_offset}"),
            ));
        }
        apply_batch_payload(payload, header_end, bytes, &mut index, &mut memory)?;
        offset = record_end;
    }

    let valid_len = mapping_offset
        .checked_add(offset as u64)
        .ok_or_else(|| Error::invalid_base("overlay length overflow"))?;
    Ok(OverlayScan {
        index,
        memory,
        valid_len,
    })
}

fn apply_batch_payload(
    payload: &[u8],
    payload_offset: usize,
    mapping: &[u8],
    index: &mut OverlayMap,
    memory: &mut usize,
) -> Result<()> {
    let Some(count_bytes) = payload.get(..4) else {
        return Err(Error::from_io(
            ErrorKind::InvalidData,
            "truncated batch count",
        ));
    };
    let count = u32::from_le_bytes(count_bytes.try_into().unwrap()) as usize;
    if count > MAX_BATCH_OPERATIONS {
        return Err(Error::from_io(
            ErrorKind::InvalidData,
            "batch operation count exceeds the runtime limit",
        ));
    }
    index
        .try_reserve_mapped(count)
        .map_err(|_| Error::invalid_base("Overlay index allocation failed"))?;

    let mut cursor = 4usize;
    for _ in 0..count {
        let operation_offset = payload_offset
            .checked_add(cursor)
            .ok_or_else(|| Error::invalid_base("overlay operation offset overflow"))?;
        let header_end = cursor
            .checked_add(9)
            .ok_or_else(|| Error::invalid_base("overlay operation offset overflow"))?;
        let Some(header) = payload.get(cursor..header_end) else {
            return Err(Error::from_io(
                ErrorKind::InvalidData,
                "truncated batch operation",
            ));
        };
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
        let key_start = header_end;
        let key_end = key_start
            .checked_add(key_len)
            .ok_or_else(|| Error::invalid_base("batch key offset overflow"))?;
        let value_end = key_end
            .checked_add(value_len)
            .ok_or_else(|| Error::invalid_base("batch value offset overflow"))?;
        let Some(key) = payload.get(key_start..key_end) else {
            return Err(Error::from_io(
                ErrorKind::InvalidData,
                "truncated batch key",
            ));
        };
        if payload.get(key_end..value_end).is_none() {
            return Err(Error::from_io(
                ErrorKind::InvalidData,
                "truncated batch value",
            ));
        }

        let mutation = MappedMutation { operation_offset };
        let (removed, added) = index.install_recovered(mutation, xxh3_64(key));
        *memory = memory.saturating_sub(removed).saturating_add(added);
        cursor = value_end;
    }
    if cursor != payload.len() {
        return Err(Error::from_io(
            ErrorKind::InvalidData,
            "trailing bytes in batch",
        ));
    }
    debug_assert!(index.mapped_bytes_len() == mapping.len());
    Ok(())
}
