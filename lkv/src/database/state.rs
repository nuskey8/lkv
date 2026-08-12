use crate::error::{Error, Result};
use crate::format::log::Marker;
use crate::format::segment::{
    CHECKSUM_BLOCK_SIZE, SegmentLayout, read_base_header, segment_layout,
    segment_metadata_checksum, validate_base, verify_segment_blocks,
};
use crate::format::superblock::Superblock;
use crate::options::VerificationMode;
use crc32c::crc32c;
use hashbrown::HashTable;
use memmap2::Mmap;
use rustc_hash::FxHashMap;
use std::io::ErrorKind;
use std::ops::Deref;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use xxhash_rust::xxh3::xxh3_64;

pub type KeyMap<V> = FxHashMap<Vec<u8>, V>;
pub const MAPPED_VALUE_THRESHOLD: usize = 1024 * 1024;
pub const OVERLAY_MAPPING_THRESHOLD: usize = 1024 * 1024;

/// Immutable bytes backing an installed Base generation.
#[derive(Clone)]
pub enum BaseBytes {
    Mapped(Arc<Mmap>),
    Memory {
        bytes: Arc<Vec<u8>>,
        range: Range<usize>,
    },
}

impl Deref for BaseBytes {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Mapped(mapping) => mapping,
            Self::Memory { bytes, range } => &bytes[range.clone()],
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

const OPERATION_HEADER_SIZE: usize = 9;

#[derive(Clone, Copy)]
pub(crate) struct MappedRecord(usize);

impl MappedRecord {
    fn checked_slices(self, bytes: &[u8]) -> Option<(&[u8], Option<&[u8]>)> {
        let header_end = self.0.checked_add(OPERATION_HEADER_SIZE)?;
        let header = bytes.get(self.0..header_end)?;
        let marker = Marker::from_byte(header[0])?;
        if !matches!(marker, Marker::Put | Marker::Delete) {
            return None;
        }

        let key_len = u32::from_le_bytes(header[1..5].try_into().unwrap()) as usize;
        let value_len = u32::from_le_bytes(header[5..9].try_into().unwrap()) as usize;
        if marker == Marker::Delete && value_len != 0 {
            return None;
        }

        let key_end = header_end.checked_add(key_len)?;
        let value_end = key_end.checked_add(value_len)?;
        let key = bytes.get(header_end..key_end)?;
        let value = match marker {
            Marker::Put => Some(bytes.get(key_end..value_end)?),
            Marker::Delete => None,
            _ => unreachable!(),
        };

        Some((key, value))
    }

    #[inline]
    fn slices(self, bytes: &[u8]) -> (&[u8], Option<&[u8]>) {
        self.checked_slices(bytes)
            .expect("installed Overlay record must remain valid")
    }

    fn memory_charge(self, bytes: &[u8]) -> usize {
        let (key, value) = self.slices(bytes);
        key.len()
            + value.map_or(0, |value| {
                if value.len() < MAPPED_VALUE_THRESHOLD {
                    value.len()
                } else {
                    0
                }
            })
    }
}

#[derive(Clone, Copy)]
pub struct MappedMutation {
    pub operation_offset: usize,
}

impl MappedMutation {
    fn into_record(self) -> MappedRecord {
        MappedRecord(self.operation_offset)
    }
}

#[derive(Clone)]
pub(crate) struct TailOverlayEntry {
    key: Vec<u8>,
    value: OverlayEntry,
    mapped: MappedRecord,
}

#[derive(Clone)]
pub enum OverlayMap {
    Memory(HashTable<(Vec<u8>, OverlayEntry)>),
    Mapped {
        bytes: Option<BaseBytes>,
        table: HashTable<MappedRecord>,
        tail: HashTable<TailOverlayEntry>,
    },
}

impl OverlayMap {
    pub fn memory() -> Self {
        Self::Memory(HashTable::new())
    }

    pub fn mapped(bytes: Option<BaseBytes>) -> Self {
        Self::Mapped {
            bytes,
            table: HashTable::new(),
            tail: HashTable::new(),
        }
    }

    #[inline]
    pub fn get(&self, key: &[u8]) -> Option<Option<&[u8]>> {
        self.get_hashed(key, xxh3_64(key))
    }

    #[inline]
    pub fn get_hashed(&self, key: &[u8], hash: u64) -> Option<Option<&[u8]>> {
        match self {
            Self::Memory(table) => table
                .find(hash, |(candidate, _)| candidate.as_slice() == key)
                .map(|(_, entry)| match entry {
                    OverlayEntry::Put(value) => Some(value.as_slice()),
                    OverlayEntry::Delete => None,
                }),
            Self::Mapped { bytes, table, tail } => {
                if let Some(entry) = tail.find(hash, |entry| entry.key.as_slice() == key) {
                    return Some(match &entry.value {
                        OverlayEntry::Put(value) => Some(value.as_slice()),
                        OverlayEntry::Delete => None,
                    });
                }
                let bytes = bytes.as_deref()?;
                table
                    .find(hash, |record| record.slices(bytes).0 == key)
                    .map(|record| record.slices(bytes).1)
            }
        }
    }

    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.get(key).is_some()
    }

    pub fn iter(&self) -> OverlayIter<'_> {
        match self {
            Self::Memory(table) => OverlayIter::Memory(table.iter()),
            Self::Mapped { bytes, table, tail } => OverlayIter::Mapped {
                bytes: bytes.as_deref().unwrap_or(&[]),
                mapped: table.iter(),
                tail: tail.iter(),
            },
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Memory(table) => table.len(),
            Self::Mapped { table, tail, .. } => table.len() + tail.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn memory_charge(&self) -> usize {
        match self {
            Self::Memory(table) => table
                .iter()
                .map(|(key, entry)| overlay_entry_memory(key, entry))
                .sum(),
            Self::Mapped { bytes, table, tail } => {
                let bytes = bytes.as_deref().unwrap_or(&[]);
                table
                    .iter()
                    .map(|record| record.memory_charge(bytes))
                    .sum::<usize>()
                    + tail
                        .iter()
                        .map(|entry| overlay_entry_memory(&entry.key, &entry.value))
                        .sum::<usize>()
            }
        }
    }

    fn set_memory(&mut self, key: Vec<u8>, value: OverlayEntry) -> Option<OverlayEntry> {
        let Self::Memory(table) = self else {
            unreachable!("file Overlay cannot install owned entries")
        };
        let hash = xxh3_64(&key);
        if let Some((_, previous)) = table.find_mut(hash, |(candidate, _)| candidate == &key) {
            return Some(std::mem::replace(previous, value));
        }
        table.insert_unique(hash, (key, value), |(key, _)| xxh3_64(key));
        None
    }

    fn install_mapped(
        &mut self,
        bytes: BaseBytes,
        mutations: impl IntoIterator<Item = MappedMutation>,
    ) -> (usize, usize) {
        let Self::Mapped {
            bytes: current,
            table,
            tail,
        } = self
        else {
            unreachable!("memory Overlay cannot install mapped entries")
        };
        *current = Some(bytes);
        let mapping = current.as_deref().unwrap();
        let previous_tail = std::mem::replace(tail, HashTable::new());
        for entry in previous_tail {
            debug_assert_eq!(entry.mapped.slices(mapping).0, entry.key.as_slice());
            let previous = insert_mapped_record(table, mapping, entry.mapped);
            debug_assert!(previous.is_none());
        }
        let mut removed = 0usize;
        let mut added = 0usize;
        for mutation in mutations {
            let record = mutation.into_record();
            added = added.saturating_add(record.memory_charge(mapping));
            if let Some(previous) = insert_mapped_record(table, mapping, record) {
                removed = removed.saturating_add(previous.memory_charge(mapping));
            }
        }
        (removed, added)
    }

    fn install_tail(
        &mut self,
        staged: KeyMap<OverlayEntry>,
        mutations: Vec<MappedMutation>,
    ) -> (usize, usize) {
        let Self::Mapped { bytes, table, tail } = self else {
            unreachable!("memory Overlay cannot install a file tail")
        };
        let mapping = bytes.as_deref();
        let mut removed = 0usize;
        let mut added = 0usize;
        for ((key, value), mutation) in staged.into_iter().zip(mutations) {
            let mapped = mutation.into_record();
            let hash = xxh3_64(&key);
            added = added.saturating_add(overlay_entry_memory(&key, &value));
            if let Some(entry) = tail.find_mut(hash, |entry| entry.key == key) {
                removed = removed.saturating_add(overlay_entry_memory(&entry.key, &entry.value));
                entry.value = value;
                entry.mapped = mapped;
                continue;
            }
            if let Some(old_charge) = mapping.and_then(|mapping| {
                table
                    .find_entry(hash, |record| record.slices(mapping).0 == key.as_slice())
                    .ok()
                    .map(|entry| entry.remove().0.memory_charge(mapping))
            }) {
                removed = removed.saturating_add(old_charge);
            }
            tail.insert_unique(hash, TailOverlayEntry { key, value, mapped }, |entry| {
                xxh3_64(&entry.key)
            });
        }
        (removed, added)
    }

    pub fn install_recovered(&mut self, mutation: MappedMutation, hash: u64) -> (usize, usize) {
        let Self::Mapped { bytes, table, .. } = self else {
            unreachable!("memory Overlay cannot install recovered entries")
        };
        let mapping = bytes.as_deref().unwrap();
        let record = mutation.into_record();
        debug_assert!(record.checked_slices(mapping).is_some());
        let added = record.memory_charge(mapping);
        let removed = insert_mapped_record_hashed(table, mapping, record, hash)
            .map_or(0, |record| record.memory_charge(mapping));
        (removed, added)
    }

    pub fn replace_mapped_bytes(&mut self, bytes: Option<BaseBytes>) {
        let Self::Mapped { bytes: current, .. } = self else {
            unreachable!("memory Overlay cannot replace mapped bytes")
        };
        *current = bytes;
    }

    pub fn mapped_bytes_len(&self) -> usize {
        match self {
            Self::Mapped { bytes, .. } => bytes.as_deref().map_or(0, <[u8]>::len),
            Self::Memory(_) => 0,
        }
    }

    fn clear(&mut self) {
        match self {
            Self::Memory(table) => *table = HashTable::new(),
            Self::Mapped { bytes, table, tail } => {
                *bytes = None;
                *table = HashTable::new();
                *tail = HashTable::new();
            }
        }
    }

    pub fn try_reserve_mapped(
        &mut self,
        additional: usize,
    ) -> std::result::Result<(), hashbrown::TryReserveError> {
        let Self::Mapped { bytes, table, .. } = self else {
            unreachable!("memory Overlay cannot reserve mapped entries")
        };
        let mapping = bytes.as_deref().unwrap_or(&[]);
        table.try_reserve(additional, |record| xxh3_64(record.slices(mapping).0))
    }

    #[cfg(test)]
    pub fn mapped_bytes(&self) -> Option<&BaseBytes> {
        match self {
            Self::Mapped { bytes, .. } => bytes.as_ref(),
            Self::Memory(_) => None,
        }
    }
}

fn insert_mapped_record(
    table: &mut HashTable<MappedRecord>,
    mapping: &[u8],
    record: MappedRecord,
) -> Option<MappedRecord> {
    let key = record.slices(mapping).0;
    let hash = xxh3_64(key);
    insert_mapped_record_hashed(table, mapping, record, hash)
}

fn insert_mapped_record_hashed(
    table: &mut HashTable<MappedRecord>,
    mapping: &[u8],
    record: MappedRecord,
    hash: u64,
) -> Option<MappedRecord> {
    let key = record.slices(mapping).0;
    if let Some(existing) = table.find_mut(hash, |candidate| candidate.slices(mapping).0 == key) {
        Some(std::mem::replace(existing, record))
    } else {
        table.insert_unique(hash, record, |candidate| {
            xxh3_64(candidate.slices(mapping).0)
        });
        None
    }
}

pub enum OverlayIter<'a> {
    Memory(hashbrown::hash_table::Iter<'a, (Vec<u8>, OverlayEntry)>),
    Mapped {
        bytes: &'a [u8],
        mapped: hashbrown::hash_table::Iter<'a, MappedRecord>,
        tail: hashbrown::hash_table::Iter<'a, TailOverlayEntry>,
    },
}

impl<'a> Iterator for OverlayIter<'a> {
    type Item = (&'a [u8], Option<&'a [u8]>);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Memory(inner) => inner.next().map(|(key, entry)| {
                let value = match entry {
                    OverlayEntry::Put(value) => Some(value.as_slice()),
                    OverlayEntry::Delete => None,
                };
                (key.as_slice(), value)
            }),
            Self::Mapped {
                bytes,
                mapped,
                tail,
            } => mapped.next().map_or_else(
                || {
                    tail.next().map(|entry| {
                        let value = match &entry.value {
                            OverlayEntry::Put(value) => Some(value.as_slice()),
                            OverlayEntry::Delete => None,
                        };
                        (entry.key.as_slice(), value)
                    })
                },
                |record| Some(record.slices(bytes)),
            ),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::Memory(inner) => inner.size_hint(),
            Self::Mapped { mapped, tail, .. } => {
                let len = mapped.len() + tail.len();
                (len, Some(len))
            }
        }
    }
}

impl ExactSizeIterator for OverlayIter<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Memory(inner) => inner.len(),
            Self::Mapped { mapped, tail, .. } => mapped.len() + tail.len(),
        }
    }
}

fn overlay_entry_memory(key: &[u8], entry: &OverlayEntry) -> usize {
    key.len()
        + match entry {
            OverlayEntry::Put(value) => value.overlay_memory_charge(),
            OverlayEntry::Delete => 0,
        }
}

pub struct OverlayState {
    pub index: Arc<OverlayMap>,
    pub memory: usize,
}

impl OverlayState {
    pub fn new(index: OverlayMap) -> Self {
        let memory = index.memory_charge();
        Self {
            index: Arc::new(index),
            memory,
        }
    }

    pub fn with_memory(index: OverlayMap, memory: usize) -> Self {
        Self {
            index: Arc::new(index),
            memory,
        }
    }

    pub fn set(&mut self, key: Vec<u8>, entry: OverlayEntry) {
        let charge = overlay_entry_memory(&key, &entry);
        let key_len = key.len();
        let index = Arc::make_mut(&mut self.index);
        if let Some(previous) = index.set_memory(key, entry) {
            self.memory = self
                .memory
                .saturating_sub(key_len + previous_value_memory(&previous));
        }
        self.memory = self.memory.saturating_add(charge);
    }

    pub fn install_mapped(&mut self, bytes: BaseBytes, mutations: Vec<MappedMutation>) {
        let (removed, added) = Arc::make_mut(&mut self.index).install_mapped(bytes, mutations);
        self.memory = self.memory.saturating_sub(removed).saturating_add(added);
    }

    pub fn needs_mapping(&self, overlay_size: usize) -> bool {
        overlay_size.saturating_sub(self.index.mapped_bytes_len()) >= OVERLAY_MAPPING_THRESHOLD
    }

    pub fn install_tail(&mut self, staged: KeyMap<OverlayEntry>, mutations: Vec<MappedMutation>) {
        let (removed, added) = Arc::make_mut(&mut self.index).install_tail(staged, mutations);
        self.memory = self.memory.saturating_sub(removed).saturating_add(added);
    }

    pub fn clear(&mut self) {
        Arc::make_mut(&mut self.index).clear();
        self.memory = 0;
    }
}

fn previous_value_memory(entry: &OverlayEntry) -> usize {
    match entry {
        OverlayEntry::Put(value) => value.overlay_memory_charge(),
        OverlayEntry::Delete => 0,
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

#[cfg(test)]
mod tests {
    use super::{MappedRecord, OverlayEntry, OverlayMap, ValueBytes};
    use xxhash_rust::xxh3::xxh3_64;

    #[test]
    fn overlay_map_uses_the_base_key_hash() {
        assert_eq!(
            std::mem::size_of::<MappedRecord>(),
            std::mem::size_of::<usize>()
        );
        let mut map = OverlayMap::memory();
        map.set_memory(Vec::new(), OverlayEntry::Put(ValueBytes::Owned(vec![1])));
        map.set_memory(
            b"overlay-key".to_vec(),
            OverlayEntry::Put(ValueBytes::Owned(vec![2])),
        );

        for (key, expected) in [
            (b"".as_slice(), &[1][..]),
            (b"overlay-key".as_slice(), &[2][..]),
        ] {
            let hash = xxh3_64(key);
            assert_eq!(map.get(key), Some(Some(expected)));
            assert_eq!(map.get_hashed(key, hash), Some(Some(expected)));
        }

        let occupied_hash = xxh3_64(b"overlay-key");
        assert_eq!(map.get_hashed(b"other-key", occupied_hash), None);
    }
}
