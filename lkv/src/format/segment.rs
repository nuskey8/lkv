use super::{u32_at, u64_at};
use crate::{Error, Result};
use crc32c::crc32c;
use memmap2::{Mmap, MmapOptions};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, ErrorKind, Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use xxhash_rust::xxh3::xxh3_64;

pub const BASE_HEADER: usize = 24;
pub const SLOT_SIZE: usize = 12;
pub const CHECKSUM_BLOCK_SIZE: usize = 64 * 1024;
pub const CHECKSUM_FOOTER_SIZE: usize = 24;
pub const EMPTY_SEGMENT_SIZE: u64 =
    BASE_HEADER as u64 + size_of::<u32>() as u64 + CHECKSUM_FOOTER_SIZE as u64;
pub const MAX_KEY_SIZE: usize = 1024 * 1024;
pub const MAX_VALUE_SIZE: usize = u32::MAX as usize;

const HASH_SECTION_ID: &[u8; 4] = b"HASH";
const CHECKSUM_SECTION_ID: &[u8; 4] = b"CRC3";
const WRITE_BUFFER_SIZE: usize = 256 * 1024;

pub fn check_lengths(key: &[u8], value: &[u8]) -> Result<()> {
    if key.len() > MAX_KEY_SIZE || value.len() > MAX_VALUE_SIZE {
        Err(Error::from_io(
            ErrorKind::InvalidInput,
            format!(
                "key or value exceeds the format limit ({MAX_KEY_SIZE} key bytes, {MAX_VALUE_SIZE} value bytes)"
            ),
        ))
    } else {
        Ok(())
    }
}

pub fn record_at(mapping: &[u8], offset: usize, data_size: usize) -> Result<(&[u8], &[u8], usize)> {
    let header_end = offset
        .checked_add(8)
        .filter(|end| *end <= data_size)
        .ok_or_else(|| Error::invalid_base("truncated record header"))?;
    let header = mapping
        .get(offset..header_end)
        .ok_or_else(|| Error::invalid_base("record header lies outside mapping"))?;
    let key_len = u32_at(header, 0) as usize;
    let value_len = u32_at(header, 4) as usize;
    let value_start = header_end
        .checked_add(key_len)
        .ok_or_else(|| Error::invalid_base("record key length overflow"))?;
    let end = value_start
        .checked_add(value_len)
        .filter(|end| *end <= data_size)
        .ok_or_else(|| Error::invalid_base("record extends beyond segment"))?;
    let key = mapping
        .get(header_end..value_start)
        .ok_or_else(|| Error::invalid_base("record key lies outside mapping"))?;
    let value = mapping
        .get(value_start..end)
        .ok_or_else(|| Error::invalid_base("record value lies outside mapping"))?;
    Ok((key, value, end))
}

struct WrittenSegment {
    pub size: u64,
    pub metadata_checksum: u32,
}

pub(crate) struct WrittenBase {
    pub size: u64,
    pub slots: u64,
    pub len: usize,
    pub metadata_checksum: u32,
}

pub fn map(file: &File, offset: u64, size: u64) -> Result<Arc<Mmap>> {
    if size == 0 {
        return Err(Error::from_io(
            ErrorKind::InvalidData,
            "cannot map an empty segment",
        ));
    }
    let end = offset
        .checked_add(size)
        .ok_or_else(|| Error::invalid_base("segment range overflows file offsets"))?;
    if end > file.metadata()?.len() {
        return Err(Error::invalid_base(
            "segment extends beyond the database file",
        ));
    }
    let len = usize::try_from(size)
        .map_err(|_| Error::from_io(ErrorKind::InvalidData, "segment is too large to map"))?;
    // SAFETY: callers keep mapped ranges immutable for the mapping's lifetime.
    // New data is appended, and the exclusive file lock prevents concurrent mutation.
    unsafe {
        Ok(Arc::new(
            MmapOptions::new().offset(offset).len(len).map(file)?,
        ))
    }
}

#[inline]
pub fn trusted_record_at(mapping: &[u8], offset: usize) -> (&[u8], &[u8], usize) {
    let key_len = u32_at(mapping, offset) as usize;
    let value_len = u32_at(mapping, offset + 4) as usize;
    let key_start = offset + 8;
    let value_start = key_start + key_len;
    let end = value_start + value_len;
    (
        &mapping[key_start..value_start],
        &mapping[value_start..end],
        end,
    )
}

pub fn write_base_at<'a, S: Read + Write + Seek>(
    file: &mut S,
    start: u64,
    len: usize,
    entries: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
) -> Result<u64> {
    Ok(write_base_with_metadata_at(file, start, len, entries)?.size)
}

pub(crate) fn write_base_with_metadata_at<'a, S: Read + Write + Seek>(
    file: &mut S,
    start: u64,
    len: usize,
    entries: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
) -> Result<WrittenBase> {
    let slots = base_slot_count(len)?;
    let mut index = vec![(0u32, 0u64); slots as usize];
    let index_bytes = (slots as usize)
        .checked_mul(SLOT_SIZE)
        .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "base index is too large"))?;
    let records_start = start
        .checked_add(BASE_HEADER as u64)
        .and_then(|offset| offset.checked_add(index_bytes as u64))
        .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "base offset overflow"))?;
    file.seek(SeekFrom::Start(records_start))?;
    let mut writer = BufWriter::with_capacity(WRITE_BUFFER_SIZE, &mut *file);
    let mut offset = records_start;
    let mut written_len = 0usize;
    for (key, value) in entries {
        written_len += 1;
        check_lengths(key, value)?;
        writer.write_all(&(key.len() as u32).to_le_bytes())?;
        writer.write_all(&(value.len() as u32).to_le_bytes())?;
        writer.write_all(key)?;
        writer.write_all(value)?;
        let h = xxh3_64(key);
        let mut slot = h & (slots - 1);
        while index[slot as usize].1 != 0 {
            slot = (slot + 1) & (slots - 1);
        }
        index[slot as usize] = ((h >> 32) as u32, offset);
        offset = offset
            .checked_add(8 + key.len() as u64 + value.len() as u64)
            .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "base is too large"))?;
    }
    writer.flush()?;
    drop(writer);
    if written_len != len {
        return Err(Error::from_io(
            ErrorKind::InvalidInput,
            "entry count changed while writing base",
        ));
    }
    file.seek(SeekFrom::Start(start))?;
    let mut writer = BufWriter::with_capacity(WRITE_BUFFER_SIZE, &mut *file);
    writer.write_all(HASH_SECTION_ID)?;
    writer.write_all(&(BASE_HEADER as u32).to_le_bytes())?;
    writer.write_all(&slots.to_le_bytes())?;
    writer.write_all(&(len as u64).to_le_bytes())?;
    if slots > 0 {
        for (fingerprint, offset) in index {
            writer.write_all(&fingerprint.to_le_bytes())?;
            writer.write_all(&offset.to_le_bytes())?;
        }
    }
    writer.flush()?;
    drop(writer);
    let written = append_block_checksums(file, start, offset - start)?;
    Ok(WrittenBase {
        size: written.size,
        slots,
        len,
        metadata_checksum: written.metadata_checksum,
    })
}

fn append_block_checksums(
    file: &mut (impl Read + Write + Seek),
    start: u64,
    data_size: u64,
) -> Result<WrittenSegment> {
    let block_count = data_size.div_ceil(CHECKSUM_BLOCK_SIZE as u64) as usize;
    let mut checksums = Vec::with_capacity(block_count);
    let mut buffer = vec![0; CHECKSUM_BLOCK_SIZE];
    file.seek(SeekFrom::Start(start))?;
    let mut remaining = data_size;
    while remaining > 0 {
        let len = remaining.min(CHECKSUM_BLOCK_SIZE as u64) as usize;
        file.read_exact(&mut buffer[..len])?;
        checksums.push(crc32c(&buffer[..len]));
        remaining -= len as u64;
    }

    let metadata = checksum_metadata(data_size, &checksums)?;
    let metadata_checksum = crc32c(&metadata);
    file.seek(SeekFrom::Start(start + data_size))?;
    file.write_all(&metadata)?;
    let size = data_size
        .checked_add(metadata.len() as u64)
        .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "segment is too large"))?;
    Ok(WrittenSegment {
        size,
        metadata_checksum,
    })
}

fn checksum_metadata(data_size: u64, checksums: &[u32]) -> Result<Vec<u8>> {
    let table_size = checksums
        .len()
        .checked_mul(size_of::<u32>())
        .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "checksum table is too large"))?;
    let metadata_size = table_size
        .checked_add(CHECKSUM_FOOTER_SIZE)
        .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "checksum table is too large"))?;
    let mut metadata = Vec::new();
    metadata
        .try_reserve_exact(metadata_size)
        .map_err(|_| Error::from_io(ErrorKind::InvalidInput, "checksum table is too large"))?;
    for checksum in checksums {
        metadata.extend_from_slice(&checksum.to_le_bytes());
    }
    let table_crc = crc32c(&metadata);
    metadata.extend_from_slice(CHECKSUM_SECTION_ID);
    metadata.extend_from_slice(&(CHECKSUM_FOOTER_SIZE as u32).to_le_bytes());
    metadata.extend_from_slice(&data_size.to_le_bytes());
    metadata.extend_from_slice(&(CHECKSUM_BLOCK_SIZE as u32).to_le_bytes());
    metadata.extend_from_slice(&table_crc.to_le_bytes());
    Ok(metadata)
}

pub struct SegmentLayout<'a> {
    data: &'a [u8],
    checksums: &'a [u8],
}

impl<'a> SegmentLayout<'a> {
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    pub fn checksums(&self) -> &'a [u8] {
        self.checksums
    }
}

pub fn segment_layout(bytes: &[u8]) -> Result<SegmentLayout<'_>> {
    if bytes.len() < CHECKSUM_FOOTER_SIZE {
        return Err(Error::invalid_base("missing checksum footer"));
    }
    let footer = &bytes[bytes.len() - CHECKSUM_FOOTER_SIZE..];
    if &footer[..4] != CHECKSUM_SECTION_ID {
        return Err(Error::invalid_base("invalid checksum footer"));
    }
    if u32_at(footer, 4) as usize != CHECKSUM_FOOTER_SIZE {
        return Err(Error::invalid_base("invalid checksum footer size"));
    }
    let data_size = usize::try_from(u64_at(footer, 8))
        .map_err(|_| Error::invalid_base("segment data size overflow"))?;
    if u32_at(footer, 16) as usize != CHECKSUM_BLOCK_SIZE {
        return Err(Error::invalid_base("unsupported checksum block size"));
    }
    let checksum_count = data_size.div_ceil(CHECKSUM_BLOCK_SIZE);
    let checksum_size = checksum_count
        .checked_mul(4)
        .ok_or_else(|| Error::invalid_base("checksum table overflow"))?;
    if data_size
        .checked_add(checksum_size)
        .and_then(|size| size.checked_add(CHECKSUM_FOOTER_SIZE))
        != Some(bytes.len())
    {
        return Err(Error::invalid_base(
            "checksum layout does not match segment size",
        ));
    }
    let checksums = &bytes[data_size..data_size + checksum_size];
    if crc32c(checksums) != u32_at(footer, 20) {
        return Err(Error::corrupted_metadata("checksum table CRC32C mismatch"));
    }
    Ok(SegmentLayout {
        data: &bytes[..data_size],
        checksums,
    })
}

pub fn verify_segment_blocks(layout: &SegmentLayout<'_>, segment_offset: u64) -> Result<()> {
    for (index, block) in layout.data.chunks(CHECKSUM_BLOCK_SIZE).enumerate() {
        if crc32c(block) != u32_at(layout.checksums, index * 4) {
            return Err(Error::corrupted_block(segment_offset, index as u64));
        }
    }
    Ok(())
}

pub fn segment_metadata_checksum(bytes: &[u8]) -> Result<u32> {
    let layout = segment_layout(bytes)?;
    Ok(crc32c(&bytes[layout.data.len()..]))
}

pub fn measure_base_iter<'a>(
    entries: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
) -> Result<(usize, u64)> {
    let mut records_size = BASE_HEADER;
    let mut observed = 0usize;
    for (key, value) in entries {
        observed = observed
            .checked_add(1)
            .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "too many entries"))?;
        records_size = records_size
            .checked_add(8)
            .and_then(|size| size.checked_add(key.len()))
            .and_then(|size| size.checked_add(value.len()))
            .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "base is too large"))?;
    }
    let slots = usize::try_from(base_slot_count(observed)?)
        .map_err(|_| Error::from_io(ErrorKind::InvalidInput, "too many entries"))?;
    let size = records_size
        .checked_add(
            slots
                .checked_mul(SLOT_SIZE)
                .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "base is too large"))?,
        )
        .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "base is too large"))?;
    let checksum_size = size
        .div_ceil(CHECKSUM_BLOCK_SIZE)
        .checked_mul(4)
        .and_then(|checksums| checksums.checked_add(CHECKSUM_FOOTER_SIZE))
        .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "base is too large"))?;
    let total = size
        .checked_add(checksum_size)
        .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "base is too large"))?;
    let total = u64::try_from(total)
        .map_err(|_| Error::from_io(ErrorKind::InvalidInput, "base is too large"))?;
    Ok((observed, total))
}

fn base_slot_count(len: usize) -> Result<u64> {
    if len == 0 {
        return Ok(0);
    }
    // Base is built only during compaction, so spend the extra construction
    // work to keep the frozen index at or below an 80% load factor.
    len.checked_mul(5)
        .map(|scaled| scaled.div_ceil(4))
        .and_then(usize::checked_next_power_of_two)
        .and_then(|slots| u64::try_from(slots).ok())
        .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "too many entries"))
}

pub fn read_base_header(bytes: &[u8]) -> Result<(u64, usize)> {
    if bytes.len() < BASE_HEADER || &bytes[..4] != HASH_SECTION_ID {
        return Err(Error::from_io(ErrorKind::InvalidData, "invalid base file"));
    }
    if u32_at(bytes, 4) as usize != BASE_HEADER {
        return Err(Error::from_io(
            ErrorKind::InvalidData,
            "invalid base header size",
        ));
    }
    let slots = u64_at(bytes, 8);
    let len = usize::try_from(u64_at(bytes, 16))
        .map_err(|_| Error::from_io(ErrorKind::InvalidData, "base entry count overflow"))?;
    if slots != 0 && !slots.is_power_of_two() {
        return Err(Error::from_io(ErrorKind::InvalidData, "invalid slot count"));
    }
    records_start(slots, bytes.len())?;
    Ok((slots, len))
}

pub fn records_start(slots: u64, data_size: usize) -> Result<usize> {
    let slots = usize::try_from(slots).map_err(|_| Error::invalid_base("slot count overflow"))?;
    let start = slots
        .checked_mul(SLOT_SIZE)
        .and_then(|size| BASE_HEADER.checked_add(size))
        .ok_or_else(|| Error::invalid_base("hash index size overflow"))?;
    if start > data_size {
        return Err(Error::invalid_base("truncated hash index"));
    }
    Ok(start)
}

pub fn validate_base(bytes: &[u8], absolute_start: u64, slots: u64, len: usize) -> Result<()> {
    let records_start = records_start(slots, bytes.len())?;
    if len > bytes.len().saturating_sub(records_start) / 8 {
        return Err(Error::invalid_base("entry count exceeds segment capacity"));
    }
    let mut offset = records_start;
    let mut records = HashMap::new();
    let mut keys = HashSet::new();
    records
        .try_reserve(len)
        .map_err(|_| Error::invalid_base("record verification allocation failed"))?;
    keys.try_reserve(len)
        .map_err(|_| Error::invalid_base("key verification allocation failed"))?;
    for _ in 0..len {
        let header_end = offset
            .checked_add(8)
            .ok_or_else(|| Error::invalid_base("record overflow"))?;
        if header_end > bytes.len() {
            return Err(Error::invalid_base("truncated record header"));
        }
        let key_len = u32_at(bytes, offset) as usize;
        let value_len = u32_at(bytes, offset + 4) as usize;
        let key_start = header_end;
        let value_start = key_start
            .checked_add(key_len)
            .ok_or_else(|| Error::invalid_base("key length overflow"))?;
        let end = value_start
            .checked_add(value_len)
            .ok_or_else(|| Error::invalid_base("value length overflow"))?;
        if end > bytes.len() {
            return Err(Error::invalid_base("record extends beyond base"));
        }
        let key = &bytes[key_start..value_start];
        if !keys.insert(key) {
            return Err(Error::invalid_base("duplicate key"));
        }
        records.insert(absolute_start + offset as u64, (xxh3_64(key) >> 32) as u32);
        offset = end;
    }
    if offset != bytes.len() {
        return Err(Error::invalid_base("unreferenced bytes at end of base"));
    }
    let mut referenced = HashSet::new();
    referenced
        .try_reserve(len)
        .map_err(|_| Error::invalid_base("index verification allocation failed"))?;
    for slot in 0..slots as usize {
        let slot_offset = BASE_HEADER + slot * SLOT_SIZE;
        let stored_fingerprint = u32_at(bytes, slot_offset);
        let record_offset = u64_at(bytes, slot_offset + 4);
        if record_offset == 0 {
            if stored_fingerprint != 0 {
                return Err(Error::invalid_base("empty slot has a fingerprint"));
            }
            continue;
        }
        if records.get(&record_offset) != Some(&stored_fingerprint) {
            return Err(Error::invalid_base(
                "slot points outside base or has the wrong fingerprint",
            ));
        }
        if !referenced.insert(record_offset) {
            return Err(Error::invalid_base(
                "record is referenced by multiple slots",
            ));
        }
    }
    if referenced.len() != len {
        return Err(Error::invalid_base("record is missing from hash index"));
    }
    Ok(())
}
