use super::state::{ActiveBase, BaseBytes, OverlayState};
use super::transaction::RawEntries;
use super::view::{BaseView, ReadView};
use super::{Database, HandleState, failpoints};
use crate::error::{Error, Result};
use crate::format::log::{LOG_HEADER_SIZE, write_compact_marker};
use crate::format::segment::EMPTY_SEGMENT_SIZE;
use crate::format::segment::{
    measure_base_iter, read_base_header, segment_layout, segment_metadata_checksum, write_base_at,
    write_base_with_metadata_at,
};
use crate::format::superblock::{self, DATA_START, Superblock};
use crate::options::VerificationMode;
use std::io::{self, Cursor, ErrorKind, Read, Seek, SeekFrom, Write};
use std::sync::Arc;

impl Database {
    /// Rebuilds all live entries and shrinks the storage image in place.
    ///
    /// All snapshots must be dropped before this operation.
    pub fn compact(&mut self) -> Result<()> {
        self.ensure_writable()?;
        if Arc::strong_count(&self.snapshot_guard) != 1 {
            return Err(Error::from_io(
                ErrorKind::WouldBlock,
                "compact requires all snapshots to be dropped",
            ));
        }
        self.verify()?;
        if self.base.offset == DATA_START
            && self.overlay.index.is_empty()
            && self.storage.len()? == self.base.log_start
        {
            return Ok(());
        }
        let (live_len, expected_base_size) = measure_base_iter(self.iter_raw())?;
        if self.storage.is_memory() {
            return self.compact_memory(live_len, expected_base_size);
        }
        self.compact_file(live_len, expected_base_size)
    }

    fn compact_file(&mut self, live_len: usize, expected_base_size: u64) -> Result<()> {
        let rollback_offset = self.storage.seek(SeekFrom::End(0))?;
        let destination_end = DATA_START
            .checked_add(expected_base_size)
            .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "compact size overflow"))?;
        let marker_end = rollback_offset
            .checked_add(LOG_HEADER_SIZE as u64)
            .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "compact marker overflow"))?;
        // Keep the staging Base beyond both the current file and the final
        // marker. This lets either Superblock remain recoverable at every
        // crash point, even when the final Base is larger than the old image.
        let source_offset = marker_end.max(
            destination_end
                .checked_add(LOG_HEADER_SIZE as u64)
                .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "compact gap overflow"))?,
        );
        let source_end = source_offset
            .checked_add(expected_base_size)
            .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "compact size overflow"))?;
        self.ensure_capacity(source_end - rollback_offset)?;

        if let Err(error) = write_compact_marker(&mut self.storage) {
            return self.rollback_or_poison(rollback_offset, error);
        }
        failpoints::crash_process_if_requested("after_compact_marker_write");
        if let Err(error) = self.storage.seek(SeekFrom::Start(source_offset)) {
            return self.rollback_or_poison(rollback_offset, error.into());
        }
        let entries = raw_entries(&self.base, &self.overlay, self.options.verification);
        let written = match write_base_with_metadata_at(
            &mut self.storage,
            source_offset,
            live_len,
            entries,
        ) {
            Ok(written) => written,
            Err(error) => return self.rollback_or_poison(rollback_offset, error),
        };
        if written.size != expected_base_size {
            return self.rollback_or_poison(
                rollback_offset,
                Error::other("base size changed while compacting"),
            );
        }
        failpoints::crash_process_if_requested("after_compact_base_write");
        if let Err(error) = self.storage.sync_data() {
            return self.rollback_or_poison(rollback_offset, error);
        }
        failpoints::crash_process_if_requested("after_compact_base_sync");

        let source_superblock = Superblock::new(
            self.base.generation + 1,
            source_offset,
            written.size,
            written.slots,
            written.len as u64,
            source_end,
            written.metadata_checksum,
        );
        if let Err(error) = superblock::write(&mut self.storage, source_superblock) {
            self.state = HandleState::WritePoisoned;
            return Err(error);
        }
        failpoints::crash_process_if_requested("after_compact_superblock_write");
        if let Err(error) = self.storage.sync_all() {
            self.state = HandleState::WritePoisoned;
            return Err(error);
        }
        failpoints::crash_process_if_requested("after_compact_superblock_sync");
        self.install_superblock(source_superblock)?;
        self.overlay.clear();

        // Prepare the temporary unmapped Base before publishing the relocated Base.
        // If this allocation fails, both the handle and the durable Superblock still refer to the staging Base,
        // so later writes remain recoverable after reopening.
        let destination_generation = source_superblock.generation() + 1;
        let detached = detached_base(destination_generation)?;

        let entries = raw_entries(&self.base, &self.overlay, self.options.verification);
        let destination_written =
            write_base_with_metadata_at(&mut self.storage, DATA_START, live_len, entries)?;
        if destination_written.size != expected_base_size {
            return Err(Error::other(
                "base size changed while relocating compaction output",
            ));
        }
        failpoints::crash_process_if_requested("after_compact_destination_base_write");
        self.storage.seek(SeekFrom::Start(destination_end))?;
        write_compact_marker(&mut self.storage)?;
        failpoints::crash_process_if_requested("after_compact_relocation_marker_write");
        self.storage.sync_data()?;
        failpoints::crash_process_if_requested("after_compact_relocation_sync");

        let destination_superblock = Superblock::new(
            destination_generation,
            DATA_START,
            destination_written.size,
            destination_written.slots,
            destination_written.len as u64,
            destination_end,
            destination_written.metadata_checksum,
        );
        if let Err(error) = superblock::write(&mut self.storage, destination_superblock) {
            self.state = HandleState::WritePoisoned;
            return Err(error);
        }
        failpoints::crash_process_if_requested("after_compact_destination_superblock_write");
        if let Err(error) = self.storage.sync_all() {
            self.state = HandleState::WritePoisoned;
            return Err(error);
        }
        failpoints::crash_process_if_requested("after_compact_destination_superblock_sync");
        if let Err(error) = superblock::write_redundant(&mut self.storage, destination_superblock) {
            self.state = HandleState::WritePoisoned;
            return Err(error);
        }
        failpoints::crash_process_if_requested("after_compact_redundant_superblock_write");
        if let Err(error) = self.storage.sync_all() {
            self.state = HandleState::WritePoisoned;
            return Err(error);
        }
        failpoints::crash_process_if_requested("after_compact_redundant_superblock_sync");

        // SetEndOfFile fails on Windows while any section of the file is mapped.
        // No snapshot exists, and Overlay mappings were released when the staging Base was installed,
        // so replacing our final Base mapping is sufficient on every supported OS.
        self.state = HandleState::Unavailable;
        self.base = detached;
        self.storage.set_len(destination_end)?;
        failpoints::crash_process_if_requested("after_compact_truncate");
        self.storage.sync_all()?;
        failpoints::crash_process_if_requested("after_compact_truncate_sync");
        if let Err(error) = self.install_superblock(destination_superblock) {
            self.state = HandleState::Unavailable;
            return Err(error);
        }
        self.state = HandleState::Healthy;
        Ok(())
    }

    fn compact_memory(&mut self, live_len: usize, expected_base_size: u64) -> Result<()> {
        let required = DATA_START
            .checked_add(expected_base_size)
            .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "compact size overflow"))?;
        if required > self.options.max_database_bytes {
            return Err(Error::database_full(
                self.options.max_database_bytes,
                required,
            ));
        }

        let bytes = build_memory_base(DATA_START, expected_base_size, live_len, self.iter_raw())?;
        if bytes.len() as u64 != expected_base_size {
            return Err(Error::other("base size changed while compacting"));
        }
        self.install_memory_base(DATA_START, bytes, live_len)
    }

    fn install_memory_base(
        &mut self,
        base_offset: u64,
        bytes: Arc<Vec<u8>>,
        live_len: usize,
    ) -> Result<()> {
        let mapping = BaseBytes::Memory {
            range: 0..bytes.len(),
            bytes: Arc::clone(&bytes),
        };
        let (base_slots, base_len) = read_base_header(segment_layout(&mapping)?.data())?;
        if base_len != live_len {
            return Err(Error::other("in-memory Base entry count changed"));
        }
        let base_checksum = segment_metadata_checksum(&mapping)?;
        let log_start = base_offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "Base range overflow"))?;
        let superblock = Superblock::new(
            self.base.generation + 1,
            base_offset,
            bytes.len() as u64,
            base_slots,
            base_len as u64,
            log_start,
            base_checksum,
        );
        let installed = ActiveBase::install(mapping, superblock)?;
        if let Err(error) =
            superblock::write(&mut self.storage, superblock).and_then(|()| self.storage.sync_all())
        {
            self.state = HandleState::WritePoisoned;
            return Err(error);
        }
        if let Err(error) = self.storage.replace_memory_base(base_offset, bytes) {
            self.state = HandleState::WritePoisoned;
            return Err(error);
        }
        self.finish_superblock_install(Ok(installed))?;
        self.overlay.clear();
        Ok(())
    }
}

fn detached_base(generation: u64) -> Result<ActiveBase> {
    let bytes = build_memory_base(DATA_START, EMPTY_SEGMENT_SIZE, 0, std::iter::empty())?;
    let mapping = BaseBytes::Memory {
        range: 0..bytes.len(),
        bytes,
    };
    let checksum = segment_metadata_checksum(&mapping)?;
    ActiveBase::install(
        mapping,
        Superblock::new(
            generation,
            DATA_START,
            EMPTY_SEGMENT_SIZE,
            0,
            0,
            DATA_START + EMPTY_SEGMENT_SIZE,
            checksum,
        ),
    )
}

fn raw_entries<'a>(
    base: &'a ActiveBase,
    overlay: &'a OverlayState,
    verification: VerificationMode,
) -> RawEntries<'a> {
    RawEntries::new(
        ReadView {
            base: BaseView {
                mapping: &base.mapping,
                verifier: &base.verifier,
                offset: base.offset,
                slots: base.slots,
            },
            overlay: &overlay.index,
            verification,
        },
        base.len,
    )
}

fn build_memory_base<'a>(
    offset: u64,
    expected_size: u64,
    len: usize,
    entries: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
) -> Result<Arc<Vec<u8>>> {
    let mut buffer = OffsetBuffer::new(offset, expected_size)?;
    let size = write_base_at(&mut buffer, offset, len, entries)?;
    let bytes = buffer.into_inner();
    if bytes.len() as u64 != size {
        return Err(Error::other(
            "in-memory Base size does not match its buffer",
        ));
    }
    Ok(Arc::new(bytes))
}

struct OffsetBuffer {
    origin: u64,
    inner: Cursor<Vec<u8>>,
}

impl OffsetBuffer {
    fn new(origin: u64, expected_size: u64) -> Result<Self> {
        let capacity = usize::try_from(expected_size)
            .map_err(|_| Error::from_io(ErrorKind::InvalidInput, "Base is too large"))?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| Error::from_io(ErrorKind::OutOfMemory, "could not reserve Base bytes"))?;
        Ok(Self {
            origin,
            inner: Cursor::new(bytes),
        })
    }

    fn into_inner(self) -> Vec<u8> {
        self.inner.into_inner()
    }

    fn absolute(&self, relative: u64) -> io::Result<u64> {
        self.origin
            .checked_add(relative)
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "Base position overflow"))
    }
}

impl Read for OffsetBuffer {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buffer)
    }
}

impl Write for OffsetBuffer {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Seek for OffsetBuffer {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let relative = match position {
            SeekFrom::Start(position) => position.checked_sub(self.origin).ok_or_else(|| {
                io::Error::new(ErrorKind::InvalidInput, "Base seek precedes its origin")
            })?,
            SeekFrom::End(offset) => {
                let relative = self.inner.seek(SeekFrom::End(offset))?;
                return self.absolute(relative);
            }
            SeekFrom::Current(offset) => {
                let relative = self.inner.seek(SeekFrom::Current(offset))?;
                return self.absolute(relative);
            }
        };
        let relative = self.inner.seek(SeekFrom::Start(relative))?;
        self.absolute(relative)
    }
}
