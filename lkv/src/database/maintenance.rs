//! Logical compaction, physical vacuuming, and stale-vacuum handling.

use super::state::{ActiveBase, BaseBytes, OverlayState};
use super::storage::Storage;
use super::transaction::RawEntries;
use super::view::{BaseView, ReadView};
use super::{Database, HandleState, failpoints, sync_directory};
#[cfg(windows)]
use super::{load_storage_state, open_database_file};
use crate::error::{Error, Result};
use crate::format::log::{LOG_HEADER_SIZE, write_compact_marker};
#[cfg(windows)]
use crate::format::segment::EMPTY_SEGMENT_SIZE;
use crate::format::segment::{
    measure_base_iter, read_base_header, segment_layout, segment_metadata_checksum, write_base_at,
    write_base_with_metadata_at,
};
use crate::format::superblock::{self, DATA_START, Superblock};
use crate::options::VerificationMode;
use fs2::FileExt;
use std::fs::{self, OpenOptions};
use std::io::{self, Cursor, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(windows)]
use std::{os::windows::ffi::OsStrExt, ptr};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

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
        let rollback_offset = self.storage.seek(SeekFrom::End(0))?;
        if let Err(error) = write_compact_marker(&mut self.storage) {
            return self.rollback_or_poison(rollback_offset, error);
        }
        let base_offset = self.storage.stream_position()?;
        let entries = raw_entries(&self.base, &self.overlay, self.options.verification);
        let base_size = match write_base_at(&mut self.storage, base_offset, live_len, entries) {
            Ok(size) => size,
            Err(error) => return self.rollback_or_poison(rollback_offset, error),
        };
        if base_size != expected_base_size {
            return self.rollback_or_poison(
                rollback_offset,
                Error::other("base size changed while compacting"),
            );
        }
        if let Err(error) = self.storage.sync_data() {
            return self.rollback_or_poison(rollback_offset, error);
        }
        failpoints::crash_process_if_requested("after_compact_base_sync");
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
            self.state = HandleState::WritePoisoned;
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
    /// Unlike [`Database::compact`], this physically shrinks it.
    ///
    /// All snapshots must be dropped before this operation.
    pub fn vacuum(&mut self) -> Result<()> {
        self.ensure_writable()?;
        self.verify()?;
        if Arc::strong_count(&self.snapshot_guard) != 1 {
            return Err(Error::from_io(
                ErrorKind::WouldBlock,
                "vacuum requires all snapshots to be dropped",
            ));
        }
        if self.storage.is_memory() {
            return self.vacuum_memory();
        }
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
        #[cfg(windows)]
        let backup = vacuum_backup_path(&path);
        #[cfg(windows)]
        ensure_vacuum_path_available(&backup)?;
        let mut new_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        FileExt::try_lock_exclusive(&new_file)?;
        new_file.set_len(DATA_START)?;
        let written =
            write_base_with_metadata_at(&mut new_file, DATA_START, live_len, self.iter_raw())?;
        if written.size != expected_base_size {
            return Err(Error::other("base size changed while vacuuming"));
        }
        let superblock = Superblock::new(
            self.base.generation + 1,
            DATA_START,
            written.size,
            written.slots,
            written.len as u64,
            DATA_START + written.size,
            written.metadata_checksum,
        );
        superblock::write(&mut new_file, superblock)?;
        new_file.sync_all()?;
        failpoints::crash_process_if_requested("after_vacuum_file_sync");

        #[cfg(not(windows))]
        fs::rename(&temporary, &path)?;

        #[cfg(windows)]
        {
            // Windows refuses replacement while either file has an open mapped
            // section. Install a tiny inaccessible placeholder while every
            // database mapping and both file handles are released.
            let detached = detached_base(self.base.generation)?;
            FileExt::unlock(&new_file)?;
            if let Storage::File(file) = &self.storage {
                FileExt::unlock(file)?;
            }
            self.state = HandleState::Unavailable;
            self.overlay.clear();
            self.base = detached;
            let old_storage = std::mem::replace(&mut self.storage, Storage::memory());
            drop(old_storage);
            drop(new_file);
            if let Err(error) = replace_file(&path, &temporary, &backup) {
                if matches!(fs::symlink_metadata(&path), Err(error) if error.kind() == ErrorKind::NotFound)
                {
                    let _ = fs::rename(&backup, &path);
                }
                let _ = self.restore_windows_database(&path);
                return Err(error.into());
            }
        }

        failpoints::crash_process_if_requested("after_vacuum_rename");

        #[cfg(not(windows))]
        {
            self.storage = Storage::File(new_file);
            self.install_superblock(superblock)?;
            self.overlay.clear();
        }

        #[cfg(windows)]
        {
            self.install_windows_vacuum(&path, superblock)?;
            fs::remove_file(&backup)?;
        }
        if let Some(parent) = path.parent()
            && let Err(error) = sync_directory(parent)
        {
            self.state = HandleState::WritePoisoned;
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

    #[cfg(windows)]
    fn install_windows_vacuum(&mut self, path: &Path, superblock: Superblock) -> Result<()> {
        let file = open_database_file(path)?;
        let mut storage = Storage::File(file);
        let mapping = storage.load_immutable(superblock.base_offset(), superblock.base_size())?;
        let base = ActiveBase::install(mapping, superblock)?;
        storage.seek(SeekFrom::End(0))?;
        self.storage = storage;
        self.base = base;
        self.overlay.clear();
        self.state = HandleState::Healthy;
        Ok(())
    }

    #[cfg(windows)]
    fn restore_windows_database(&mut self, path: &Path) -> Result<()> {
        let file = open_database_file(path)?;
        let mut storage = Storage::File(file);
        let (base, overlay) = load_storage_state(&mut storage, &self.options)?;
        self.storage = storage;
        self.base = base;
        self.overlay = overlay;
        self.state = HandleState::Healthy;
        Ok(())
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

    /// Returns whether a regular temporary or backup file remains from an
    /// interrupted vacuum. Symlinks and other special files are rejected.
    pub fn has_stale_vacuum(&self) -> Result<bool> {
        let Some(path) = self.path.as_ref() else {
            return Ok(false);
        };
        for temporary in [compacting_path(path), vacuum_backup_path(path)] {
            match fs::symlink_metadata(&temporary) {
                Ok(metadata) if metadata.file_type().is_file() => return Ok(true),
                Ok(_) => {
                    return Err(Error::from_io(
                        ErrorKind::InvalidInput,
                        "vacuum temporary path is not a regular file",
                    ));
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(false)
    }

    /// Removes a regular stale vacuum file. The file is locked first so this
    /// does not remove a vacuum temporary file actively owned by another handle.
    pub fn remove_stale_vacuum(&self) -> Result<bool> {
        let Some(path) = self.path.as_ref() else {
            return Ok(false);
        };
        let mut removed = false;
        for temporary in [compacting_path(path), vacuum_backup_path(path)] {
            removed |= remove_stale_vacuum_file(&temporary)?;
        }
        if removed && let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
        Ok(removed)
    }
}

fn remove_stale_vacuum_file(path: &Path) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
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
    let stale = OpenOptions::new().read(true).write(true).open(path)?;
    FileExt::try_lock_exclusive(&stale).map_err(|error| {
        Error::from_io(
            ErrorKind::WouldBlock,
            format!("vacuum temporary file is in use: {error}"),
        )
    })?;
    let locked_metadata = fs::symlink_metadata(path)?;
    if !locked_metadata.file_type().is_file() {
        return Err(Error::from_io(
            ErrorKind::InvalidInput,
            "vacuum temporary path changed while being inspected",
        ));
    }
    FileExt::unlock(&stale)?;
    drop(stale);
    fs::remove_file(path)?;
    Ok(true)
}

#[cfg(windows)]
fn ensure_vacuum_path_available(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(Error::from_io(
            ErrorKind::AlreadyExists,
            "stale vacuum backup already exists",
        )),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn replace_file(replaced: &Path, replacement: &Path, backup: &Path) -> io::Result<()> {
    let replaced = nul_terminated_path(replaced)?;
    let replacement = nul_terminated_path(replacement)?;
    let backup = nul_terminated_path(backup)?;
    // SAFETY: all paths are NUL-terminated UTF-16 strings and the remaining
    // pointer arguments are reserved and required to be null.
    if unsafe {
        ReplaceFileW(
            replaced.as_ptr(),
            replacement.as_ptr(),
            backup.as_ptr(),
            0,
            ptr::null(),
            ptr::null(),
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn nul_terminated_path(path: &Path) -> io::Result<Vec<u16>> {
    let mut path: Vec<u16> = path.as_os_str().encode_wide().collect();
    if path.contains(&0) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "path contains an interior NUL",
        ));
    }
    path.push(0);
    Ok(path)
}

#[cfg(windows)]
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

pub fn compacting_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".compacting");
    PathBuf::from(name)
}

pub(super) fn vacuum_backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".vacuum-backup");
    PathBuf::from(name)
}
