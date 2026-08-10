mod maintenance;
mod recovery;
mod state;
mod storage;
mod transaction;
mod view;

use recovery::scan_overlay;

use crate::error::{Error, Result};
use crate::format::log::{LOG_HEADER_SIZE, batch_payload_len};
use crate::format::segment::{self, EMPTY_SEGMENT_SIZE, segment_metadata_checksum, write_base_at};
use crate::format::superblock::{self, DATA_START, Superblock};
use crate::options::DatabaseOptions;
use fs2::FileExt;
use state::{ActiveBase, MAPPED_VALUE_THRESHOLD};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, ErrorKind, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use storage::Storage;
use transaction::RawEntries;
use view::{BaseView, ReadView};

pub use state::{BaseBytes, KeyMap, OverlayEntry, OverlayState, ValueBytes};
pub use transaction::{Entries, ReadTransaction, ReservedValue, Snapshot, WriteTransaction};

const LOG_WRITE_BUFFER_SIZE: usize = 64 * 1024;
const MIN_DATABASE_BYTES: u64 = DATA_START + EMPTY_SEGMENT_SIZE;
static NEXT_CREATION_FILE: AtomicU64 = AtomicU64::new(0);

fn validate_options(options: &DatabaseOptions) -> Result<()> {
    if options.max_database_bytes < MIN_DATABASE_BYTES {
        Err(Error::from_io(
            ErrorKind::InvalidInput,
            format!("max_database_bytes must be at least {MIN_DATABASE_BYTES} bytes"),
        ))
    } else {
        Ok(())
    }
}

/// Constant-time structural and maintenance metrics for a database.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct DatabaseStats {
    /// Total bytes in the file or logical volatile serialized image.
    pub storage_bytes: u64,
    /// Bytes occupied by the active immutable Base segment.
    pub base_bytes: u64,
    /// Entries physically present in the active Base, including entries
    /// shadowed by the Overlay.
    pub base_entries: usize,
    /// Latest Overlay keys, including tombstones.
    pub overlay_entries: usize,
    /// Bytes occupied by active Overlay records in the logical serialized image.
    pub overlay_log_bytes: u64,
    /// Soft memory charge for active Overlay keys and inline-sized values.
    /// Recovered inline values remain charged even when mmap-backed so the
    /// maintenance threshold is stable across reopen.
    pub overlay_memory_bytes: usize,
    /// Bytes belonging to obsolete generations before the active Base.
    pub stale_bytes: u64,
    /// Active Superblock generation.
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HandleState {
    Healthy,
    WritePoisoned,
    Unavailable,
}

/// Represents an open lkv database.
///
/// ## Examples
///
/// ```no_run
/// use lkv::Database;
///
/// # fn main() -> lkv::Result<()> {
/// let mut db = Database::create("example.lkv")?;
///
/// let mut write = db.begin_write()?;
/// write.put("key", "value")?;
/// write.commit()?;
///
/// let read = db.begin_read()?;
/// let value = read.get("key")?.unwrap();
/// assert_eq!(value, b"value");
/// # Ok(())
/// # }
/// ```
///
pub struct Database {
    storage: Storage,
    base: ActiveBase,
    overlay: OverlayState,
    options: DatabaseOptions,
    snapshot_guard: Arc<()>,
    state: HandleState,
}

impl Drop for Database {
    fn drop(&mut self) {
        if let Storage::File(file) = &self.storage {
            // A concurrently spawned child may briefly inherit a duplicate of
            // this descriptor. Explicitly release the flock before closing so
            // an immediate reopen does not wait for that duplicate to close.
            let _ = FileExt::unlock(file);
        }
    }
}

impl Database {
    fn read_view(&self) -> ReadView<'_> {
        ReadView {
            base: BaseView {
                mapping: &self.base.mapping,
                verifier: &self.base.verifier,
                offset: self.base.offset,
                slots: self.base.slots,
            },
            overlay: &self.overlay.index,
            verification: self.options.verification,
        }
    }

    /// Opens an existing lkv database.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_options(path, DatabaseOptions::default())
    }

    /// Opens an existing database with custom options.
    pub fn open_with_options(path: impl AsRef<Path>, options: DatabaseOptions) -> Result<Self> {
        validate_options(&options)?;
        let path = path.as_ref().to_path_buf();
        let file = open_database_file(&path)?;
        let initial_len = file.metadata()?.len();
        if initial_len > options.max_database_bytes {
            return Err(Error::database_full(
                options.max_database_bytes,
                initial_len,
            ));
        }
        Self::from_storage(Storage::File(file), options)
    }

    /// Creates and opens a new empty database.
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        Self::create_with_options(path, DatabaseOptions::default())
    }

    /// Creates and opens a new empty database with custom options.
    pub fn create_with_options(path: impl AsRef<Path>, options: DatabaseOptions) -> Result<Self> {
        validate_options(&options)?;
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                return Err(Error::from_io(
                    ErrorKind::AlreadyExists,
                    "database already exists",
                ));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let file = create_database_file(&path)?;
        Self::from_storage(Storage::File(file), options)
    }

    /// Creates a new database in memory.
    pub fn memory() -> Result<Self> {
        Self::memory_with_options(DatabaseOptions::default())
    }

    /// Creates a new database in memory with custom options.
    pub fn memory_with_options(options: DatabaseOptions) -> Result<Self> {
        validate_options(&options)?;
        let mut storage = Storage::memory();
        initialize_storage(&mut storage)?;
        Self::from_storage(storage, options)
    }

    fn from_storage(mut storage: Storage, options: DatabaseOptions) -> Result<Self> {
        let storage_len = storage.len()?;
        if storage_len > options.max_database_bytes {
            return Err(Error::database_full(
                options.max_database_bytes,
                storage_len,
            ));
        }
        let (base, overlay) = load_storage_state(&mut storage, &options)?;

        Ok(Self {
            storage,
            base,
            overlay,
            options,
            snapshot_guard: Arc::new(()),
            state: HandleState::Healthy,
        })
    }

    /// Performs the complete integrity verification used by a Full open.
    pub fn verify(&self) -> Result<()> {
        self.ensure_available()?;
        self.base.verify()
    }

    /// Begins a read transaction.
    pub fn begin_read(&self) -> Result<ReadTransaction<'_>> {
        self.ensure_available()?;
        Ok(ReadTransaction::new(self))
    }

    /// Creates a snapshot of the current database state.
    ///
    /// The snapshot is a point-in-time view of the database that may outlive
    /// the writer handle and be read concurrently with later writes.
    ///
    /// ## Examples
    ///
    /// ```no_run
    /// use lkv::Database;
    ///
    /// # fn main() -> lkv::Result<()> {
    /// let mut db = Database::open("path/to/db.lkv")?;
    /// let snapshot = db.snapshot()?;
    ///
    /// let mut write = db.begin_write()?;
    /// write.put(b"key", b"value")?;
    /// assert_eq!(snapshot.get(b"key")?, None);
    /// # Ok(())
    /// # }
    /// ```
    pub fn snapshot(&self) -> Result<Snapshot> {
        self.ensure_available()?;
        Ok(Snapshot::new(
            self.base.mapping.clone(),
            self.options.verification,
            Arc::clone(&self.base.verifier),
            Arc::clone(&self.overlay.index),
            (self.base.offset, self.base.slots, self.base.len),
            Arc::clone(&self.snapshot_guard),
        ))
    }

    /// Begins a write transaction.
    pub fn begin_write(&mut self) -> Result<WriteTransaction<'_>> {
        self.ensure_writable()?;
        if self.overlay.memory > self.options.overlay_memory_limit {
            return Err(Error::MaintenanceRequired {
                limit: self.options.overlay_memory_limit,
                actual: self.overlay.memory,
            });
        }
        Ok(WriteTransaction::new(self))
    }

    /// Returns the value for the given key.
    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<&[u8]>> {
        self.ensure_available()?;
        self.read_view().get(key.as_ref())
    }

    fn commit_staged(
        &mut self,
        staged: KeyMap<OverlayEntry>,
        value_checksums: KeyMap<u32>,
    ) -> Result<()> {
        if self.storage.is_memory() {
            self.commit_staged_memory(staged)
        } else {
            self.commit_staged_with(staged, value_checksums, append_batch)
        }
    }

    fn commit_staged_memory(&mut self, staged: KeyMap<OverlayEntry>) -> Result<()> {
        self.ensure_writable()?;
        if staged.is_empty() {
            return Ok(());
        }
        let payload_len = batch_payload_len(&staged)? as u64;
        let batch_record_len = (LOG_HEADER_SIZE as u64)
            .checked_add(payload_len)
            .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "batch record size overflow"))?;
        self.ensure_capacity(batch_record_len)?;
        self.storage.extend_memory_log(batch_record_len)?;
        for (key, entry) in staged {
            self.set_overlay_entry(key, entry);
        }
        Ok(())
    }

    fn commit_staged_with(
        &mut self,
        staged: KeyMap<OverlayEntry>,
        value_checksums: KeyMap<u32>,
        append: impl FnOnce(&mut Storage, &KeyMap<OverlayEntry>, &KeyMap<u32>) -> Result<()>,
    ) -> Result<()> {
        self.ensure_writable()?;
        if staged.is_empty() {
            return Ok(());
        }
        let payload_len = batch_payload_len(&staged)? as u64;
        let batch_record_len = (LOG_HEADER_SIZE as u64)
            .checked_add(payload_len)
            .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "batch record size overflow"))?;
        self.ensure_capacity(batch_record_len)?;
        let rollback_offset = self.storage.seek(SeekFrom::End(0))?;
        if let Err(error) = append(&mut self.storage, &staged, &value_checksums) {
            return self.rollback_or_poison(rollback_offset, error);
        }
        let mapped = if !self.storage.is_memory()
            && staged.values().any(|entry| {
                matches!(entry, OverlayEntry::Put(value) if value.len() >= MAPPED_VALUE_THRESHOLD)
            })
        {
            let record_len = match self.storage.len().and_then(|len| {
                len.checked_sub(rollback_offset)
                    .ok_or_else(|| Error::other("storage shrank while committing a batch"))
            }) {
                Ok(len) => len,
                Err(error) => return self.rollback_or_poison(rollback_offset, error),
            };
            match self.storage.load_immutable(rollback_offset, record_len) {
                Ok(mapping) => Some(mapping),
                Err(error) => return self.rollback_or_poison(rollback_offset, error),
            }
        } else {
            None
        };
        let committed = match committed_entries(staged, mapped) {
            Ok(entries) => entries,
            Err(error) => return self.rollback_or_poison(rollback_offset, error),
        };
        failpoints::crash_process_if_requested("after_batch_write");
        if let Err(error) = self.storage.sync_data() {
            // Windows cannot truncate a range while it is still mapped.
            drop(committed);
            return self.rollback_or_poison(rollback_offset, error);
        }
        failpoints::crash_process_if_requested("after_batch_sync");
        for (key, entry) in committed {
            self.set_overlay_entry(key, entry);
        }
        Ok(())
    }

    /// Returns whether the given key is present in the database.
    pub fn contains_key(&self, key: impl AsRef<[u8]>) -> Result<bool> {
        Ok(self.get(key)?.is_some())
    }

    /// Iterates over all live entries without copying keys or values.
    pub fn iter(&self) -> Result<Entries<'_>> {
        self.ensure_available()?;
        Ok(Entries::new(self.read_view(), self.base.len))
    }

    fn iter_raw(&self) -> RawEntries<'_> {
        RawEntries::new(self.read_view(), self.base.len)
    }

    /// Returns the number of live entries in the database.
    pub fn len(&self) -> Result<usize> {
        self.ensure_available()?;
        self.raw_len()
    }

    /// Returns `true` if the database is empty, `false` otherwise.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Returns the database statistics.
    pub fn stats(&self) -> Result<DatabaseStats> {
        self.ensure_available()?;
        let storage_bytes = self.storage.len()?;
        Ok(DatabaseStats {
            storage_bytes,
            base_bytes: self.base.mapping.len() as u64,
            base_entries: self.base.len,
            overlay_entries: self.overlay.index.len(),
            overlay_log_bytes: storage_bytes.saturating_sub(self.base.log_start),
            overlay_memory_bytes: self.overlay.memory,
            stale_bytes: self.base.offset.saturating_sub(DATA_START),
            generation: self.base.generation,
        })
    }

    fn raw_len(&self) -> Result<usize> {
        let mut entries = self.iter_raw();
        let len = entries.by_ref().count();
        if let Some(error) = entries.take_error() {
            Err(error)
        } else {
            Ok(len)
        }
    }

    /// Flushes overlay contents and metadata for a file database.
    pub fn sync(&self) -> Result<()> {
        self.ensure_available()?;
        self.storage.sync_data()
    }

    fn install_superblock(&mut self, superblock: Superblock) -> Result<()> {
        let installed = self
            .storage
            .load_immutable(superblock.base_offset(), superblock.base_size())
            .and_then(|mapping| ActiveBase::install(mapping, superblock));
        self.finish_superblock_install(installed)
    }

    fn finish_superblock_install(&mut self, installed: Result<ActiveBase>) -> Result<()> {
        let base = match installed {
            Ok(base) => base,
            Err(error) => {
                self.state = HandleState::WritePoisoned;
                return Err(error);
            }
        };
        if let Err(error) = self.storage.seek(SeekFrom::End(0)) {
            self.state = HandleState::WritePoisoned;
            return Err(error.into());
        }
        self.base = base;
        Ok(())
    }

    fn ensure_writable(&self) -> Result<()> {
        if self.state != HandleState::Healthy {
            Err(Error::Poisoned)
        } else {
            Ok(())
        }
    }

    fn ensure_available(&self) -> Result<()> {
        if self.state == HandleState::Unavailable {
            Err(Error::Poisoned)
        } else {
            Ok(())
        }
    }

    fn ensure_capacity(&self, additional: u64) -> Result<()> {
        let current = self.storage.len()?;
        let required = current
            .checked_add(additional)
            .ok_or_else(|| Error::database_full(self.options.max_database_bytes, u64::MAX))?;
        if required > self.options.max_database_bytes {
            Err(Error::database_full(
                self.options.max_database_bytes,
                required,
            ))
        } else {
            Ok(())
        }
    }

    fn set_overlay_entry(&mut self, key: Vec<u8>, entry: OverlayEntry) {
        self.overlay.set(key, entry);
    }

    pub(crate) fn storage_is_memory(&self) -> bool {
        self.storage.is_memory()
    }

    fn rollback_or_poison<T>(&mut self, offset: u64, error: Error) -> Result<T> {
        self.state = HandleState::WritePoisoned;
        if self.storage.set_len(offset).is_ok() {
            let _ = self.storage.sync_data();
        }
        let _ = self.storage.seek(SeekFrom::End(0));
        Err(error)
    }
}

fn load_storage_state(
    storage: &mut Storage,
    options: &DatabaseOptions,
) -> Result<(ActiveBase, OverlayState)> {
    let storage_len = storage.len()?;
    let superblock = superblock::read_latest_from(storage, storage_len)?;
    let mapping = storage.load_immutable(superblock.base_offset(), superblock.base_size())?;
    let mut base = ActiveBase::open(mapping, superblock, options.verification)?;
    let overlay_scan = scan_overlay(storage, base.log_start, storage_len)?;
    let valid_len = overlay_scan.valid_len();
    if valid_len < storage_len {
        // SetEndOfFile fails on Windows while any section of the file is mapped.
        // Validate first, then temporarily release our Base mapping
        // before discarding an incomplete Overlay tail.
        drop(base);
        storage.set_len(valid_len)?;
        storage.sync_data()?;
        let mapping = storage.load_immutable(superblock.base_offset(), superblock.base_size())?;
        base = ActiveBase::open(mapping, superblock, options.verification)?;
    }
    let overlay_size = valid_len
        .checked_sub(base.log_start)
        .ok_or_else(|| Error::invalid_base("overlay range precedes active Base"))?;
    let overlay_mapping = if overlay_size == 0 {
        None
    } else {
        Some(storage.load_immutable(base.log_start, overlay_size)?)
    };
    let overlay_index = overlay_scan.into_index(overlay_mapping, base.log_start)?;
    storage.seek(SeekFrom::End(0))?;
    Ok((base, OverlayState::new(overlay_index)))
}

fn committed_entries(
    mut staged: KeyMap<OverlayEntry>,
    mapping: Option<BaseBytes>,
) -> Result<KeyMap<OverlayEntry>> {
    let Some(mapping) = mapping else {
        return Ok(staged);
    };
    let bytes = &mapping[..];
    let count_end = LOG_HEADER_SIZE + 4;
    let count = bytes
        .get(LOG_HEADER_SIZE..count_end)
        .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()) as usize)
        .ok_or_else(|| Error::other("written batch is missing its operation count"))?;
    let mut cursor = count_end;
    let mut committed = KeyMap::default();
    for _ in 0..count {
        let header_end = cursor
            .checked_add(9)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| Error::other("written batch has a truncated operation"))?;
        let header = &bytes[cursor..header_end];
        let key_len = u32::from_le_bytes(header[1..5].try_into().unwrap()) as usize;
        let value_len = u32::from_le_bytes(header[5..9].try_into().unwrap()) as usize;
        let key_end = header_end
            .checked_add(key_len)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| Error::other("written batch has a truncated key"))?;
        let value_end = key_end
            .checked_add(value_len)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| Error::other("written batch has a truncated value"))?;
        let (key, entry) = staged
            .remove_entry(&bytes[header_end..key_end])
            .ok_or_else(|| Error::other("written batch key is missing from staged entries"))?;
        let entry = match entry {
            OverlayEntry::Put(value) if value.len() >= MAPPED_VALUE_THRESHOLD => {
                OverlayEntry::Put(ValueBytes::Mapped {
                    bytes: mapping.clone(),
                    range: key_end..value_end,
                })
            }
            entry => entry,
        };
        committed.insert(key, entry);
        cursor = value_end;
    }
    if cursor != bytes.len() || !staged.is_empty() {
        return Err(Error::other(
            "written batch layout does not match staged entries",
        ));
    }
    Ok(committed)
}

fn open_database_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    lock_database_file(&file)?;
    Ok(file)
}

fn create_database_file(path: &Path) -> Result<File> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    for _ in 0..16 {
        let temporary = creation_path(path);
        let mut file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        if let Err(error) = lock_database_file(&file) {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        if let Err(error) = initialize_file(&mut file) {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        failpoints::crash_process_if_requested("after_creation_file_sync");
        match fs::hard_link(&temporary, path) {
            Ok(()) => {
                failpoints::crash_process_if_requested("after_creation_link");
                sync_directory(parent)?;
                failpoints::crash_process_if_requested("after_creation_directory_sync");
                fs::remove_file(&temporary)?;
                sync_directory(parent)?;
                return Ok(file);
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                drop(file);
                fs::remove_file(&temporary)?;
                sync_directory(parent)?;
                return Err(error.into());
            }
            Err(error) => {
                drop(file);
                let _ = fs::remove_file(&temporary);
                return Err(error.into());
            }
        }
    }
    Err(Error::from_io(
        ErrorKind::AlreadyExists,
        "could not allocate a unique database creation file",
    ))
}

fn lock_database_file(file: &File) -> Result<()> {
    FileExt::try_lock_exclusive(file).map_err(|error| {
        Error::from_io(
            ErrorKind::WouldBlock,
            format!("database is already open for writing: {error}"),
        )
    })
}

fn creation_path(path: &Path) -> PathBuf {
    let mut temporary = path.as_os_str().to_os_string();
    temporary.push(format!(
        ".creating-{}-{}",
        std::process::id(),
        NEXT_CREATION_FILE.fetch_add(1, Ordering::Relaxed)
    ));
    PathBuf::from(temporary)
}

fn append_batch(
    file: &mut Storage,
    staged: &KeyMap<OverlayEntry>,
    value_checksums: &KeyMap<u32>,
) -> Result<()> {
    let mut writer = BufWriter::with_capacity(LOG_WRITE_BUFFER_SIZE, file);
    crate::format::log::write_batch_record_with_checksums(&mut writer, staged, value_checksums)?;
    Ok(writer.flush()?)
}

#[cfg(not(windows))]
fn sync_directory(path: &Path) -> Result<()> {
    Ok(File::open(path)?.sync_all()?)
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> Result<()> {
    // Windows can open a directory with FILE_FLAG_BACKUP_SEMANTICS, but
    // FlushFileBuffers requires a writable file handle and does not support
    // flushing a directory handle. The database file itself is synced before
    // publishing it, so directory syncing is intentionally a no-op here.
    Ok(())
}

fn initialize_file(file: &mut File) -> Result<()> {
    file.set_len(DATA_START)?;
    let base_size = write_base_at(file, DATA_START, 0, std::iter::empty())?;
    let superblock = Superblock::new(
        1,
        DATA_START,
        base_size,
        0,
        0,
        DATA_START + base_size,
        segment_metadata_checksum(&segment::map(file, DATA_START, base_size)?)?,
    );
    superblock::write(file, superblock)?;
    Ok(file.sync_all()?)
}

fn initialize_storage(storage: &mut Storage) -> Result<()> {
    storage.set_len(DATA_START)?;
    let base_size = write_base_at(storage, DATA_START, 0, std::iter::empty())?;
    let mapping = storage.load_immutable(DATA_START, base_size)?;
    let superblock = Superblock::new(
        1,
        DATA_START,
        base_size,
        0,
        0,
        DATA_START + base_size,
        segment_metadata_checksum(&mapping)?,
    );
    superblock::write(storage, superblock)?;
    storage.sync_all()
}

#[cfg(test)]
mod failpoints {
    pub fn crash_process_if_requested(point: &str) {
        if std::env::var_os("LKV_TEST_CRASH_POINT").as_deref() == Some(std::ffi::OsStr::new(point))
        {
            // Deliberately skip destructors to model a process disappearing between
            // commit phases. Exit code 86 distinguishes the injected crash.
            std::process::exit(86);
        }
    }
}

#[cfg(not(test))]
mod failpoints {
    #[inline(always)]
    pub fn crash_process_if_requested(_: &str) {}
}

#[cfg(test)]
mod tests;
