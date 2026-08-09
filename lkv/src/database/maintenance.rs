//! Logical compaction, physical vacuuming, and stale-vacuum handling.

use super::state::{ActiveBase, BaseBytes};
use super::storage::Storage;
use super::{Database, failpoints, sync_directory};
use crate::error::{Error, Result};
use crate::format::log::{LOG_HEADER_SIZE, write_compact_marker};
use crate::format::segment::{
    self, measure_base_iter, read_base_header, segment_layout, segment_metadata_checksum,
    write_base_at,
};
use crate::format::superblock::{self, DATA_START, Superblock};
use fs2::FileExt;
use std::fs::{self, OpenOptions};
use std::io::{self, Cursor, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

impl Database {
    /// Merges all live entries into a fresh immutable base and clears the log.
    pub fn compact(&mut self) -> Result<()> {
        self.ensure_writable()?;
        self.verify()?;
        if self.storage.is_memory() {
            return self.compact_memory();
        }
        let (live_len, expected_base_size) = measure_base_iter(self.iter_raw())?;
        let additional = (LOG_HEADER_SIZE as u64)
            .checked_add(expected_base_size)
            .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "compact size overflow"))?;
        self.ensure_capacity(additional)?;
        let path = self.path.as_ref().expect("file storage has a path");
        let mut writer = OpenOptions::new().read(true).write(true).open(path)?;
        let rollback_offset = writer.seek(SeekFrom::End(0))?;
        if let Err(error) = write_compact_marker(&mut writer) {
            return self.rollback_or_poison(rollback_offset, error);
        }
        let base_offset = writer.stream_position()?;
        let base_size = match write_base_at(&mut writer, base_offset, live_len, self.iter_raw()) {
            Ok(size) => size,
            Err(error) => return self.rollback_or_poison(rollback_offset, error),
        };
        if base_size != expected_base_size {
            return self.rollback_or_poison(
                rollback_offset,
                Error::other("base size changed while compacting"),
            );
        }
        if let Err(error) = writer.sync_data() {
            return self.rollback_or_poison(rollback_offset, error.into());
        }
        failpoints::crash_process_if_requested("after_compact_base_sync");
        drop(writer);
        let mapping = self.storage.load_immutable(base_offset, base_size)?;
        let segment = &mapping[..];
        let (base_slots, base_len) = read_base_header(segment_layout(segment)?.data())?;
        let base_checksum = segment_metadata_checksum(segment)?;
        drop(mapping);
        let superblock = Superblock::new(
            self.base.generation + 1,
            base_offset,
            base_size,
            base_slots,
            base_len as u64,
            base_offset + base_size,
            base_checksum,
        );
        if let Err(error) =
            superblock::write(&mut self.storage, superblock).and_then(|()| self.storage.sync_all())
        {
            self.poisoned = true;
            return Err(error);
        }
        failpoints::crash_process_if_requested("after_compact_superblock_sync");
        self.install_superblock(superblock)?;
        self.overlay.clear();
        Ok(())
    }

    fn compact_memory(&mut self) -> Result<()> {
        let (live_len, expected_base_size) = measure_base_iter(self.iter_raw())?;
        let additional = (LOG_HEADER_SIZE as u64)
            .checked_add(expected_base_size)
            .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "compact size overflow"))?;
        self.ensure_capacity(additional)?;
        let base_offset = self
            .storage
            .len()?
            .checked_add(LOG_HEADER_SIZE as u64)
            .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "compact offset overflow"))?;
        let bytes = build_memory_base(base_offset, expected_base_size, live_len, self.iter_raw())?;
        if bytes.len() as u64 != expected_base_size {
            return Err(Error::other("base size changed while compacting"));
        }
        self.install_memory_base(base_offset, bytes, live_len)
    }

    /// Rebuilds only live entries and atomically replaces the storage image.
    /// Unlike [`Database::compact`], this physically shrinks it. File databases
    /// return [`Error::Unsupported`] on Windows; memory databases are supported.
    pub fn vacuum(&mut self) -> Result<()> {
        self.ensure_writable()?;
        self.verify()?;
        if self.storage.is_memory() {
            return self.vacuum_memory();
        }
        ensure_vacuum_supported()?;
        let (live_len, expected_base_size) = measure_base_iter(self.iter_raw())?;
        self.vacuum_precomputed(live_len, expected_base_size)
    }

    fn vacuum_precomputed(&mut self, live_len: usize, expected_base_size: u64) -> Result<()> {
        let required = DATA_START
            .checked_add(expected_base_size)
            .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "vacuum size overflow"))?;
        if required > self.options.max_database_bytes {
            return Err(Error::database_full(
                self.options.max_database_bytes,
                required,
            ));
        }
        let path = self.path.as_ref().expect("file storage has a path").clone();
        let temporary = compacting_path(&path);
        let mut new_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        FileExt::try_lock_exclusive(&new_file)?;
        new_file.set_len(DATA_START)?;
        let base_size = write_base_at(&mut new_file, DATA_START, live_len, self.iter_raw())?;
        if base_size != expected_base_size {
            return Err(Error::other("base size changed while vacuuming"));
        }
        let mapping = segment::map(&new_file, DATA_START, base_size)?;
        let segment = &mapping[..];
        let (base_slots, base_len) = read_base_header(segment_layout(segment)?.data())?;
        let base_checksum = segment_metadata_checksum(segment)?;
        drop(mapping);
        let superblock = Superblock::new(
            self.base.generation + 1,
            DATA_START,
            base_size,
            base_slots,
            base_len as u64,
            DATA_START + base_size,
            base_checksum,
        );
        superblock::write(&mut new_file, superblock)?;
        new_file.sync_all()?;
        failpoints::crash_process_if_requested("after_vacuum_file_sync");
        fs::rename(&temporary, &path)?;
        failpoints::crash_process_if_requested("after_vacuum_rename");
        self.storage = Storage::File(new_file);
        self.install_superblock(superblock)?;
        self.overlay.clear();
        if let Some(parent) = path.parent()
            && let Err(error) = sync_directory(parent)
        {
            self.poisoned = true;
            return Err(error);
        }
        Ok(())
    }

    fn vacuum_memory(&mut self) -> Result<()> {
        let (live_len, expected_base_size) = measure_base_iter(self.iter_raw())?;
        let required = DATA_START
            .checked_add(expected_base_size)
            .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "vacuum size overflow"))?;
        if required > self.options.max_database_bytes {
            return Err(Error::database_full(
                self.options.max_database_bytes,
                required,
            ));
        }

        let bytes = build_memory_base(DATA_START, expected_base_size, live_len, self.iter_raw())?;
        if bytes.len() as u64 != expected_base_size {
            return Err(Error::other("base size changed while vacuuming"));
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
            self.poisoned = true;
            return Err(error);
        }
        if let Err(error) = self.storage.replace_memory_base(base_offset, bytes) {
            self.poisoned = true;
            return Err(error);
        }
        self.finish_superblock_install(Ok(installed))?;
        self.overlay.clear();
        Ok(())
    }

    /// Returns whether a regular `<database path>.compacting` file remains from an
    /// interrupted vacuum. Symlinks and other special files are rejected.
    pub fn has_stale_vacuum(&self) -> Result<bool> {
        let Some(path) = self.path.as_ref() else {
            return Ok(false);
        };
        let temporary = compacting_path(path);
        match fs::symlink_metadata(&temporary) {
            Ok(metadata) if metadata.file_type().is_file() => Ok(true),
            Ok(_) => Err(Error::from_io(
                ErrorKind::InvalidInput,
                "vacuum temporary path is not a regular file",
            )),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Removes a regular stale vacuum file. The file is locked first so this
    /// does not remove a vacuum temporary file actively owned by another handle.
    pub fn remove_stale_vacuum(&self) -> Result<bool> {
        let Some(path) = self.path.as_ref() else {
            return Ok(false);
        };
        let temporary = compacting_path(path);
        let metadata = match fs::symlink_metadata(&temporary) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_file() {
            return Err(Error::from_io(
                ErrorKind::InvalidInput,
                "vacuum temporary path is not a regular file",
            ));
        }
        let stale = OpenOptions::new().read(true).write(true).open(&temporary)?;
        FileExt::try_lock_exclusive(&stale).map_err(|error| {
            Error::from_io(
                ErrorKind::WouldBlock,
                format!("vacuum temporary file is in use: {error}"),
            )
        })?;
        let locked_metadata = fs::symlink_metadata(&temporary)?;
        if !locked_metadata.file_type().is_file() {
            return Err(Error::from_io(
                ErrorKind::InvalidInput,
                "vacuum temporary path changed while being inspected",
            ));
        }
        drop(stale);
        fs::remove_file(&temporary)?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
        Ok(true)
    }
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

#[cfg(not(windows))]
fn ensure_vacuum_supported() -> Result<()> {
    Ok(())
}

#[cfg(windows)]
fn ensure_vacuum_supported() -> Result<()> {
    Err(Error::from_io(
        ErrorKind::Unsupported,
        "vacuum is not supported on Windows",
    ))
}

pub fn compacting_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".compacting");
    PathBuf::from(name)
}
