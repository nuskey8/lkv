use super::segment::BASE_HEADER;
use super::{u32_at, u64_at};
use crate::{Error, Result};
use crc32c::crc32c;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};

pub const MAGIC: &[u8; 8] = b"LKV\0\0\0\0\0";
pub const FORMAT_VERSION: u32 = 1;
pub const SUPERBLOCK_SIZE: u64 = 4096;
pub const HEADER_SIZE: usize = 72;
pub const DATA_START: u64 = SUPERBLOCK_SIZE * 2;
const ENVELOPE_MIN_SIZE: usize = 28;

#[derive(Clone, Copy, Debug)]
pub struct Superblock {
    generation: u64,
    base_offset: u64,
    base_size: u64,
    base_slots: u64,
    base_len: u64,
    log_start: u64,
    base_checksum: u32,
}

impl Superblock {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        generation: u64,
        base_offset: u64,
        base_size: u64,
        base_slots: u64,
        base_len: u64,
        log_start: u64,
        base_checksum: u32,
    ) -> Self {
        Self {
            generation,
            base_offset,
            base_size,
            base_slots,
            base_len,
            log_start,
            base_checksum,
        }
    }

    pub fn generation(self) -> u64 {
        self.generation
    }
    pub fn base_offset(self) -> u64 {
        self.base_offset
    }
    pub fn base_size(self) -> u64 {
        self.base_size
    }
    pub fn base_slots(self) -> u64 {
        self.base_slots
    }
    pub fn base_len(self) -> u64 {
        self.base_len
    }
    pub fn log_start(self) -> u64 {
        self.log_start
    }
    pub fn base_checksum(self) -> u32 {
        self.base_checksum
    }
}

pub fn write(file: &mut (impl Write + Seek), superblock: Superblock) -> Result<()> {
    let page = encode_page(superblock);
    let offset = (superblock.generation & 1) * SUPERBLOCK_SIZE;
    file.seek(SeekFrom::Start(offset))?;
    Ok(file.write_all(&page)?)
}

fn encode_page(superblock: Superblock) -> [u8; SUPERBLOCK_SIZE as usize] {
    let mut page = [0u8; SUPERBLOCK_SIZE as usize];
    page[..8].copy_from_slice(MAGIC);
    page[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    page[12..16].copy_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
    page[16..24].copy_from_slice(&superblock.generation.to_le_bytes());
    page[24..32].copy_from_slice(&superblock.base_offset.to_le_bytes());
    page[32..40].copy_from_slice(&superblock.base_size.to_le_bytes());
    page[40..48].copy_from_slice(&superblock.base_slots.to_le_bytes());
    page[48..56].copy_from_slice(&superblock.base_len.to_le_bytes());
    page[56..64].copy_from_slice(&superblock.log_start.to_le_bytes());
    page[64..68].copy_from_slice(&superblock.base_checksum.to_le_bytes());
    let checksum = crc32c(&page[..68]);
    page[68..72].copy_from_slice(&checksum.to_le_bytes());
    page
}

pub(crate) fn read_latest_from(file: &mut (impl Read + Seek), file_len: u64) -> Result<Superblock> {
    let mut latest = None;
    let mut unsupported_version = None;
    for offset in [0, SUPERBLOCK_SIZE] {
        let mut page = [0u8; SUPERBLOCK_SIZE as usize];
        file.seek(SeekFrom::Start(offset))?;
        match file.read_exact(&mut page) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => continue,
            Err(error) => return Err(error.into()),
        }
        let Some((version, header_size)) = read_envelope(&page) else {
            continue;
        };
        if version != FORMAT_VERSION {
            unsupported_version = Some(version);
            continue;
        }
        if header_size != HEADER_SIZE {
            continue;
        }
        let candidate = Superblock {
            generation: u64_at(&page, 16),
            base_offset: u64_at(&page, 24),
            base_size: u64_at(&page, 32),
            base_slots: u64_at(&page, 40),
            base_len: u64_at(&page, 48),
            log_start: u64_at(&page, 56),
            base_checksum: u32_at(&page, 64),
        };
        let base_end = candidate.base_offset.checked_add(candidate.base_size);
        if candidate.base_offset < DATA_START
            || candidate.base_size < BASE_HEADER as u64
            || base_end.is_none_or(|end| end > file_len)
            || candidate.log_start < base_end.unwrap()
            || candidate.log_start > file_len
        {
            continue;
        }
        if latest.is_none_or(|current: Superblock| candidate.generation > current.generation) {
            latest = Some(candidate);
        }
    }
    if let Some(version) = unsupported_version {
        Err(Error::from_io(
            ErrorKind::Unsupported,
            format!("unsupported database format version {version}"),
        ))
    } else if let Some(latest) = latest {
        Ok(latest)
    } else {
        Err(Error::from_io(
            ErrorKind::InvalidData,
            "no valid superblock",
        ))
    }
}

/// Reads the version-independent Superblock envelope. Every format version
/// keeps magic, version, header size, and generation at their current offsets,
/// with the header CRC32C stored in its final four bytes.
fn read_envelope(page: &[u8; SUPERBLOCK_SIZE as usize]) -> Option<(u32, usize)> {
    if &page[..8] != MAGIC {
        return None;
    }
    let header_size = u32_at(page, 12) as usize;
    if !(ENVELOPE_MIN_SIZE..=page.len()).contains(&header_size) {
        return None;
    }
    let checksum_offset = header_size - size_of::<u32>();
    if u32_at(page, checksum_offset) != crc32c(&page[..checksum_offset]) {
        return None;
    }
    Some((u32_at(page, 8), header_size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    struct ReadFailure;

    impl Read for ReadFailure {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "injected superblock read failure",
            ))
        }
    }

    impl Seek for ReadFailure {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            match position {
                SeekFrom::Start(offset) => Ok(offset),
                _ => unreachable!("superblock reader only uses absolute seeks"),
            }
        }
    }

    #[test]
    fn non_eof_read_errors_are_propagated() {
        let error = read_latest_from(&mut ReadFailure, DATA_START)
            .expect_err("non-EOF I/O errors must not look like invalid metadata");
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    }

    #[test]
    fn superblock_header_bytes_are_stable() {
        let page = encode_page(Superblock::new(1, 8192, 64, 8, 2, 8256, 7));
        assert_eq!(
            &page[..HEADER_SIZE],
            &[
                0x4c, 0x4b, 0x56, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x48, 0x00,
                0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x40, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x7c, 0x03,
                0xc6, 0x1f,
            ]
        );
        assert!(page[HEADER_SIZE..].iter().all(|byte| *byte == 0));
    }
}
