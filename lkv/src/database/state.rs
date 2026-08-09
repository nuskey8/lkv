use crate::error::{Error, Result};
use crate::format::segment::{
    CHECKSUM_BLOCK_SIZE, SegmentLayout, read_base_header, segment_layout,
    segment_metadata_checksum, validate_base, verify_segment_blocks,
};
use crate::format::superblock::Superblock;
use crate::options::VerificationMode;
use crc32c::crc32c;
use memmap2::Mmap;
use rustc_hash::FxHashMap;
use std::io::ErrorKind;
use std::ops::Deref;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

pub type KeyMap<V> = FxHashMap<Vec<u8>, V>;
pub const MAPPED_VALUE_THRESHOLD: usize = 1024 * 1024;

/// Immutable bytes backing an installed Base generation.
#[derive(Clone)]
pub enum BaseBytes {
    Mapped(Arc<Mmap>),
    Memory(Arc<[u8]>),
}

impl Deref for BaseBytes {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Mapped(mapping) => mapping,
            Self::Memory(bytes) => bytes,
        }
    }
}

pub struct ActiveBase {
    // File storage maps only the immutable Base; memory storage owns an
    // immutable byte slice. Both remain valid for old snapshots.
    pub mapping: BaseBytes,
    pub verifier: Arc<SegmentVerifier>,
    pub offset: u64,
    pub checksum: u32,
    pub slots: u64,
    pub len: usize,
    pub generation: u64,
    pub log_start: u64,
}

impl ActiveBase {
    pub fn open(
        mapping: BaseBytes,
        superblock: Superblock,
        verification: VerificationMode,
    ) -> Result<Self> {
        let layout = segment_layout(&mapping)?;
        if segment_metadata_checksum(&mapping)? != superblock.base_checksum() {
            return Err(Error::from_io(
                ErrorKind::InvalidData,
                "base checksum metadata mismatch",
            ));
        }
        if verification == VerificationMode::Full {
            verify_segment_blocks(&layout, superblock.base_offset())?;
        }
        let verifier = Arc::new(SegmentVerifier::new(
            superblock.base_offset(),
            &layout,
            verification == VerificationMode::Full,
        ));
        let (slots, len) = read_base_header(layout.data())?;
        if slots != superblock.base_slots() || len as u64 != superblock.base_len() {
            return Err(Error::from_io(
                ErrorKind::InvalidData,
                "superblock and base header disagree",
            ));
        }
        match verification {
            VerificationMode::Full => {
                validate_base(layout.data(), superblock.base_offset(), slots, len)?;
            }
            VerificationMode::OnRead => {}
        }
        Ok(Self::new(mapping, verifier, superblock, slots, len))
    }

    pub fn install(mapping: BaseBytes, superblock: Superblock) -> Result<Self> {
        let layout = segment_layout(&mapping)?;
        let (slots, len) = read_base_header(layout.data())?;
        if slots != superblock.base_slots() || len as u64 != superblock.base_len() {
            return Err(Error::from_io(
                ErrorKind::InvalidData,
                "installed superblock and base header disagree",
            ));
        }
        let verifier = Arc::new(SegmentVerifier::new(
            superblock.base_offset(),
            &layout,
            true,
        ));
        Ok(Self::new(mapping, verifier, superblock, slots, len))
    }

    fn new(
        mapping: BaseBytes,
        verifier: Arc<SegmentVerifier>,
        superblock: Superblock,
        slots: u64,
        len: usize,
    ) -> Self {
        Self {
            mapping,
            verifier,
            offset: superblock.base_offset(),
            checksum: superblock.base_checksum(),
            slots,
            len,
            generation: superblock.generation(),
            log_start: superblock.log_start(),
        }
    }

    pub fn verify(&self) -> Result<()> {
        let layout = segment_layout(&self.mapping)?;
        if segment_metadata_checksum(&self.mapping)? != self.checksum {
            return Err(Error::from_io(
                ErrorKind::InvalidData,
                "base content checksum mismatch",
            ));
        }
        self.verifier
            .verify_range(&self.mapping, 0, layout.data().len())?;
        if !self.verifier.is_semantically_verified() {
            validate_base(layout.data(), self.offset, self.slots, self.len)?;
            self.verifier.mark_semantically_verified();
        }
        Ok(())
    }
}

#[derive(Clone)]
pub enum ValueBytes {
    Owned(Vec<u8>),
    Mapped {
        bytes: BaseBytes,
        range: Range<usize>,
    },
}

impl ValueBytes {
    pub fn as_slice(&self) -> &[u8] {
        match self {
            Self::Owned(value) => value,
            Self::Mapped { bytes, range } => &bytes[range.clone()],
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Owned(value) => value.len(),
            Self::Mapped { range, .. } => range.len(),
        }
    }

    fn overlay_memory_charge(&self) -> usize {
        match self {
            Self::Owned(value) => value.len(),
            Self::Mapped { range, .. } if range.len() < MAPPED_VALUE_THRESHOLD => range.len(),
            Self::Mapped { .. } => 0,
        }
    }
}

#[derive(Clone)]
pub enum OverlayEntry {
    Put(ValueBytes),
    Delete,
}

fn overlay_entry_memory(key: &[u8], entry: &OverlayEntry) -> usize {
    key.len()
        + match entry {
            OverlayEntry::Put(value) => value.overlay_memory_charge(),
            OverlayEntry::Delete => 0,
        }
}

pub struct OverlayState {
    pub index: Arc<KeyMap<OverlayEntry>>,
    pub memory: usize,
}

impl OverlayState {
    pub fn new(index: KeyMap<OverlayEntry>) -> Self {
        let memory = index
            .iter()
            .map(|(key, entry)| overlay_entry_memory(key, entry))
            .sum();
        Self {
            index: Arc::new(index),
            memory,
        }
    }

    pub fn set(&mut self, key: Vec<u8>, entry: OverlayEntry) {
        let index = Arc::make_mut(&mut self.index);
        if let Some(previous) = index.remove(key.as_slice()) {
            self.memory = self
                .memory
                .saturating_sub(overlay_entry_memory(&key, &previous));
        }
        self.memory = self
            .memory
            .saturating_add(overlay_entry_memory(&key, &entry));
        index.insert(key, entry);
    }

    pub fn clear(&mut self) {
        self.index = Arc::new(KeyMap::default());
        self.memory = 0;
    }
}

#[derive(Debug)]
pub struct SegmentVerifier {
    offset: u64,
    data_size: usize,
    checksums: Box<[u32]>,
    verified: Box<[AtomicU64]>,
    remaining: AtomicUsize,
    fully_verified: AtomicBool,
    semantically_verified: AtomicBool,
}

impl SegmentVerifier {
    pub fn new(offset: u64, layout: &SegmentLayout<'_>, fully_verified: bool) -> Self {
        let checksums: Box<[u32]> = layout
            .checksums()
            .chunks_exact(4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
            .collect();
        let words = checksums.len().div_ceil(64);
        let initial = if fully_verified { u64::MAX } else { 0 };
        Self {
            offset,
            data_size: layout.data().len(),
            checksums,
            verified: (0..words).map(|_| AtomicU64::new(initial)).collect(),
            remaining: AtomicUsize::new(if fully_verified {
                0
            } else {
                layout.checksums().len() / 4
            }),
            fully_verified: AtomicBool::new(fully_verified),
            semantically_verified: AtomicBool::new(fully_verified),
        }
    }

    pub fn verify_range(&self, mapping: &[u8], start: usize, end: usize) -> Result<()> {
        if self.fully_verified.load(Ordering::Acquire) {
            return Ok(());
        }
        if start > end {
            return Err(Error::invalid_base("read range lies outside segment"));
        }
        if end > self.data_size {
            return Err(Error::invalid_base("read range lies outside segment data"));
        }
        if start == end {
            return Ok(());
        }
        let first = start / CHECKSUM_BLOCK_SIZE;
        let last = (end - 1) / CHECKSUM_BLOCK_SIZE;
        for block in first..=last {
            let word = block / 64;
            let mask = 1u64 << (block % 64);
            if self.verified[word].load(Ordering::Acquire) & mask != 0 {
                continue;
            }
            let block_start = block * CHECKSUM_BLOCK_SIZE;
            let block_end = (block_start + CHECKSUM_BLOCK_SIZE).min(self.data_size);
            if crc32c(&mapping[block_start..block_end]) != self.checksums[block] {
                return Err(Error::corrupted_block(self.offset, block as u64));
            }
            let previous = self.verified[word].fetch_or(mask, Ordering::AcqRel);
            if previous & mask == 0 && self.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                self.fully_verified.store(true, Ordering::Release);
            }
        }
        Ok(())
    }

    pub fn data_size(&self) -> usize {
        self.data_size
    }

    pub fn is_fully_verified(&self) -> bool {
        self.fully_verified.load(Ordering::Acquire)
    }

    pub fn is_semantically_verified(&self) -> bool {
        self.semantically_verified.load(Ordering::Acquire)
    }

    pub fn mark_semantically_verified(&self) {
        self.semantically_verified.store(true, Ordering::Release);
    }
}
