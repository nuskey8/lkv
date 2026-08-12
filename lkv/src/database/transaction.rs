use super::Database;
use super::state::{
    BaseBytes, KeyMap, MAPPED_VALUE_THRESHOLD, OverlayEntry, OverlayMap, SegmentVerifier,
    ValueBytes,
};
use super::view::{BaseView, ReadView};
use crate::format::log::MAX_LOG_PAYLOAD_SIZE;
use crate::format::segment::{BASE_HEADER, SLOT_SIZE, check_lengths, record_at, trusted_record_at};
use crate::{Error, Result, VerificationMode};
use crc32c::crc32c_append;
use memmap2::MmapOptions;
use std::io::{self, ErrorKind as IoErrorKind, Write};
use std::sync::Arc;

/// Read-only transaction of the database.
pub struct ReadTransaction<'db> {
    db: &'db Database,
    view: ReadView<'db>,
}

impl<'db> ReadTransaction<'db> {
    pub(crate) fn new(db: &'db Database) -> Self {
        Self {
            db,
            view: db.read_view(),
        }
    }

    /// Returns the value for the given key.
    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<&[u8]>> {
        self.view.get(key.as_ref())
    }

    /// Returns whether the given key is present in the database.
    pub fn contains_key(&self, key: impl AsRef<[u8]>) -> Result<bool> {
        Ok(self.get(key)?.is_some())
    }

    /// Iterates over all entries in the database.
    pub fn iter(&self) -> Result<Entries<'_>> {
        self.db.iter()
    }

    /// Returns the number of entries in the database.
    pub fn len(&self) -> Result<usize> {
        self.db.len()
    }

    /// Returns whether the database is empty.
    pub fn is_empty(&self) -> Result<bool> {
        self.db.is_empty()
    }
}

/// Write transaction for the database.
pub struct WriteTransaction<'db> {
    db: &'db mut Database,
    staged: KeyMap<OverlayEntry>,
    value_checksums: KeyMap<u32>,
}

impl<'db> WriteTransaction<'db> {
    pub(crate) fn new(db: &'db mut Database) -> Self {
        Self {
            db,
            staged: KeyMap::default(),
            value_checksums: KeyMap::default(),
        }
    }

    /// Puts a key-value pair into the transaction's staging area.
    pub fn put(&mut self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<()> {
        let key = key.as_ref();
        let value = value.as_ref();
        check_lengths(key, value)?;
        check_single_put_size(key.len(), value.len())?;
        let (value, value_checksum) =
            if value.len() >= MAPPED_VALUE_THRESHOLD && !self.db.storage_is_memory() {
                let (value, checksum) = copy_value(value)?;
                (value, Some(checksum))
            } else {
                (ValueBytes::Owned(value.to_vec()), None)
            };
        if let Some(checksum) = value_checksum {
            self.value_checksums.insert(key.to_vec(), checksum);
        } else {
            self.value_checksums.remove(key);
        }
        self.staged.insert(key.to_vec(), OverlayEntry::Put(value));
        Ok(())
    }

    /// Reserves exactly `value_len` bytes and lets `write` fill them without an intermediate caller-owned buffer.
    ///
    /// ## Examples
    ///
    /// ```no_run
    /// use lkv::{Database, Result};
    /// use std::io::Write;
    ///
    /// # fn main() -> Result<()> {
    /// let mut db = Database::open("example.lkv")?;
    /// let mut write = db.begin_write()?;
    /// write.put_reserved(b"key", 1024, |v| {
    ///     v.write_all(&[0; 1024])?;
    ///     Ok(())
    /// })?;
    /// write.commit()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn put_reserved<F>(
        &mut self,
        key: impl AsRef<[u8]>,
        value_len: usize,
        write: F,
    ) -> Result<()>
    where
        F: FnOnce(&mut ReservedValue<'_>) -> Result<()>,
    {
        let key = key.as_ref();
        check_lengths_len(key, value_len)?;
        let value = if value_len >= MAPPED_VALUE_THRESHOLD && !self.db.storage_is_memory() {
            let mut mapping = MmapOptions::new().len(value_len).map_anon()?;
            let checksum;
            {
                let mut reserved = ReservedValue::new(&mut mapping, true);
                write(&mut reserved)?;
                reserved.require_complete()?;
                checksum = reserved.checksum();
            }
            (
                ValueBytes::Mapped {
                    bytes: BaseBytes::Mapped(Arc::new(mapping.make_read_only()?)),
                    range: 0..value_len,
                },
                checksum,
            )
        } else {
            let mut value = Vec::new();
            value.try_reserve_exact(value_len).map_err(|_| {
                Error::from_io(IoErrorKind::OutOfMemory, "could not reserve value bytes")
            })?;
            value.resize(value_len, 0);
            let checksum;
            {
                let mut reserved = ReservedValue::new(
                    &mut value,
                    value_len >= MAPPED_VALUE_THRESHOLD && !self.db.storage_is_memory(),
                );
                write(&mut reserved)?;
                reserved.require_complete()?;
                checksum = reserved.checksum();
            }
            (ValueBytes::Owned(value), checksum)
        };
        if let Some(checksum) = value.1 {
            self.value_checksums.insert(key.to_vec(), checksum);
        } else {
            self.value_checksums.remove(key);
        }
        self.staged.insert(key.to_vec(), OverlayEntry::Put(value.0));
        Ok(())
    }

    pub fn delete(&mut self, key: impl AsRef<[u8]>) -> Result<()> {
        let key = key.as_ref();
        check_lengths(key, &[])?;
        self.value_checksums.remove(key);
        self.staged.insert(key.to_vec(), OverlayEntry::Delete);
        Ok(())
    }

    /// Reads this transaction's latest mutation first, then the committed DB.
    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<&[u8]>> {
        let key = key.as_ref();
        if let Some(entry) = self.staged.get(key) {
            return Ok(match entry {
                OverlayEntry::Put(value) => Some(value.as_slice()),
                OverlayEntry::Delete => None,
            });
        }
        self.db.get(key)
    }

    pub fn contains_key(&self, key: impl AsRef<[u8]>) -> Result<bool> {
        Ok(self.get(key)?.is_some())
    }

    /// Commits every staged mutation as one atomic record. File databases make
    /// it durable before publication; memory databases publish it after a no-op sync.
    pub fn commit(self) -> Result<()> {
        self.db.commit_staged(self.staged, self.value_checksums)
    }

    /// Explicitly discards all staged mutations. Dropping has the same effect.
    pub fn abort(self) {}

    pub fn len(&self) -> usize {
        self.staged.len()
    }

    pub fn is_empty(&self) -> bool {
        self.staged.is_empty()
    }
}

/// A sequential writer for an exactly-sized value reservation.
pub struct ReservedValue<'a> {
    bytes: &'a mut [u8],
    position: usize,
    checksum: Option<u32>,
}

impl<'a> ReservedValue<'a> {
    fn new(bytes: &'a mut [u8], checksum: bool) -> Self {
        Self {
            bytes,
            position: 0,
            checksum: checksum.then_some(0),
        }
    }

    /// Returns the number of bytes reserved for this value.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns `true` if no bytes have been written to this value.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Returns the number of bytes written to this value.
    pub fn written(&self) -> usize {
        self.position
    }

    fn checksum(&self) -> Option<u32> {
        self.checksum
    }

    fn require_complete(&self) -> Result<()> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(Error::from_io(
                IoErrorKind::InvalidInput,
                format!(
                    "reserved value is incomplete: wrote {} of {} bytes",
                    self.position,
                    self.bytes.len()
                ),
            ))
        }
    }
}

impl Write for ReservedValue<'_> {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if input.is_empty() {
            return Ok(0);
        }
        let remaining = self.bytes.len().saturating_sub(self.position);
        if input.len() > remaining {
            return Err(io::Error::new(
                IoErrorKind::InvalidInput,
                "reserved value length exceeded",
            ));
        }
        self.bytes[self.position..self.position + input.len()].copy_from_slice(input);
        if let Some(checksum) = &mut self.checksum {
            *checksum = crc32c_append(*checksum, input);
        }
        self.position += input.len();
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn copy_value(value: &[u8]) -> Result<(ValueBytes, u32)> {
    const VALUE_COPY_CHUNK_SIZE: usize = 256 * 1024;

    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(value.len())
        .map_err(|_| Error::from_io(IoErrorKind::OutOfMemory, "could not copy value bytes"))?;
    let mut checksum = 0;
    for chunk in value.chunks(VALUE_COPY_CHUNK_SIZE) {
        bytes.extend_from_slice(chunk);
        checksum = crc32c_append(checksum, chunk);
    }
    Ok((ValueBytes::Owned(bytes), checksum))
}

fn check_lengths_len(key: &[u8], value_len: usize) -> Result<()> {
    if value_len > crate::format::segment::MAX_VALUE_SIZE {
        return Err(Error::from_io(
            IoErrorKind::InvalidInput,
            "value exceeds the format limit",
        ));
    }
    check_lengths(key, &[])?;
    check_single_put_size(key.len(), value_len)
}

fn check_single_put_size(key_len: usize, value_len: usize) -> Result<()> {
    let serialized = 4usize
        .checked_add(9)
        .and_then(|len| len.checked_add(key_len))
        .and_then(|len| len.checked_add(value_len))
        .ok_or_else(|| Error::from_io(IoErrorKind::InvalidInput, "value is too large"))?;
    if serialized <= MAX_LOG_PAYLOAD_SIZE {
        Ok(())
    } else {
        Err(Error::from_io(
            IoErrorKind::InvalidInput,
            "value cannot fit in a transaction record",
        ))
    }
}

/// Zero-copy iterator over a consistent view of the current in-process state.
pub struct RawEntries<'a> {
    view: ReadView<'a>,
    base_offset: usize,
    base_remaining: usize,
    pub base_trusted: bool,
    pending_error: Option<Error>,
    finished: bool,
    overlay: hashbrown::hash_table::Iter<'a, (Vec<u8>, OverlayEntry)>,
}

impl<'a> RawEntries<'a> {
    pub fn new(view: ReadView<'a>, base_remaining: usize) -> Self {
        Self {
            view,
            base_offset: BASE_HEADER + view.base.slots as usize * SLOT_SIZE,
            base_remaining,
            base_trusted: view.base.verifier.is_semantically_verified(),
            pending_error: None,
            finished: false,
            overlay: view.overlay_iter(),
        }
    }

    pub fn take_error(&mut self) -> Option<Error> {
        self.pending_error.take()
    }
}

impl<'a> Iterator for RawEntries<'a> {
    type Item = (&'a [u8], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        while self.base_remaining > 0 {
            let offset = self.base_offset;
            let (key, value, end) = if self.base_trusted {
                trusted_record_at(self.view.base.mapping, offset)
            } else {
                match record_at(
                    self.view.base.mapping,
                    offset,
                    self.view.base.verifier.data_size(),
                ) {
                    Ok(record) => record,
                    Err(error) => {
                        self.pending_error = Some(error);
                        self.finished = true;
                        return None;
                    }
                }
            };
            self.base_offset = end;
            self.base_remaining -= 1;
            if !self.view.overlay.contains_key(key) {
                return Some((key, value));
            }
        }
        if self.base_offset != self.view.base.verifier.data_size() {
            self.pending_error = Some(Error::invalid_base(
                "base record count does not match data size",
            ));
            self.finished = true;
            return None;
        }
        for (key, entry) in self.overlay.by_ref() {
            if let OverlayEntry::Put(value) = entry {
                return Some((key, value.as_slice()));
            }
        }
        self.finished = true;
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.base_remaining + self.overlay.len()))
    }
}

/// Fallible zero-copy iterator over a consistent database view.
pub struct Entries<'a> {
    view: ReadView<'a>,
    inner: RawEntries<'a>,
}

impl<'a> Entries<'a> {
    pub(crate) fn new(view: ReadView<'a>, base_len: usize) -> Self {
        Self {
            view,
            inner: RawEntries::new(view, base_len),
        }
    }
}

impl<'a> Iterator for Entries<'a> {
    type Item = Result<(&'a [u8], &'a [u8])>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.next() {
            Some((key, value)) => Some(self.view.verify_pair(key, value).map(|()| (key, value))),
            None => self.inner.pending_error.take().map(Err),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

/// Read-only snapshot of the database.
pub struct Snapshot {
    base: BaseBytes,
    verification: VerificationMode,
    base_verifier: Arc<SegmentVerifier>,
    pub(crate) overlay_index: Arc<OverlayMap<OverlayEntry>>,
    base_offset: u64,
    base_slots: u64,
    base_len: usize,
    _database_snapshot: Arc<()>,
}

impl Snapshot {
    pub(crate) fn new(
        base: BaseBytes,
        verification: VerificationMode,
        base_verifier: Arc<SegmentVerifier>,
        overlay_index: Arc<OverlayMap<OverlayEntry>>,
        base_metadata: (u64, u64, usize),
        database_snapshot: Arc<()>,
    ) -> Self {
        let (base_offset, base_slots, base_len) = base_metadata;
        Self {
            base,
            verification,
            base_verifier,
            overlay_index,
            base_offset,
            base_slots,
            base_len,
            _database_snapshot: database_snapshot,
        }
    }

    fn read_view(&self) -> ReadView<'_> {
        ReadView {
            base: BaseView {
                mapping: &self.base,
                verifier: &self.base_verifier,
                offset: self.base_offset,
                slots: self.base_slots,
            },
            overlay: &self.overlay_index,
            verification: self.verification,
        }
    }

    /// Returns the value for the given key.
    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<&[u8]>> {
        self.read_view().get(key.as_ref())
    }

    /// Returns whether the given key exists.
    pub fn contains_key(&self, key: impl AsRef<[u8]>) -> Result<bool> {
        Ok(self.get(key)?.is_some())
    }

    /// Iterates over the entries.
    pub fn iter(&self) -> Result<Entries<'_>> {
        Ok(Entries::new(self.read_view(), self.base_len))
    }

    /// Returns the number of entries.
    pub fn len(&self) -> Result<usize> {
        self.iter()?.try_fold(0usize, |len, item| {
            item?;
            len.checked_add(1)
                .ok_or_else(|| Error::other("snapshot entry count overflow"))
        })
    }

    /// Returns whether the snapshot is empty.
    pub fn is_empty(&self) -> Result<bool> {
        match self.iter()?.next() {
            Some(Ok(_)) => Ok(false),
            Some(Err(error)) => Err(error),
            None => Ok(true),
        }
    }
}
