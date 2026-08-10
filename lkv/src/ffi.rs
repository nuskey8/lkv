#![allow(clippy::missing_safety_doc)] // Safety contracts live in include/lkv.h.

use crate::{Database, DatabaseOptions, Error, Snapshot, VerificationMode};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::slice;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

#[allow(non_camel_case_types)]
pub type lkv_status = i32;

pub const LKV_OK: lkv_status = 0;
pub const LKV_NOT_FOUND: lkv_status = 1;
pub const LKV_BUFFER_TOO_SMALL: lkv_status = 2;
pub const LKV_INVALID_ARGUMENT: lkv_status = 3;
pub const LKV_IO_ERROR: lkv_status = 4;
pub const LKV_CORRUPTED: lkv_status = 5;
pub const LKV_UNSUPPORTED: lkv_status = 6;
pub const LKV_BUSY: lkv_status = 7;
pub const LKV_DATABASE_FULL: lkv_status = 8;
pub const LKV_MAINTENANCE_REQUIRED: lkv_status = 9;
pub const LKV_POISONED: lkv_status = 10;
pub const LKV_PANIC: lkv_status = 255;

pub const LKV_VERIFICATION_ON_READ: u32 = 0;
pub const LKV_VERIFICATION_FULL: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(non_camel_case_types)]
pub struct lkv_options {
    pub verification: u32,
    pub overlay_memory_limit: usize,
    pub max_database_bytes: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(non_camel_case_types)]
pub struct lkv_database_stats {
    pub storage_bytes: u64,
    pub base_bytes: u64,
    pub base_entries: usize,
    pub overlay_entries: usize,
    pub overlay_log_bytes: u64,
    pub overlay_memory_bytes: usize,
    pub stale_bytes: u64,
    pub generation: u64,
}

#[repr(C)]
#[allow(non_camel_case_types)]
pub struct lkv_database {
    inner: RwLock<Database>,
}

#[repr(C)]
#[allow(non_camel_case_types)]
pub struct lkv_snapshot {
    inner: Snapshot,
}

#[repr(C)]
#[allow(non_camel_case_types)]
pub struct lkv_write_batch {
    entries: HashMap<Vec<u8>, Option<Vec<u8>>>,
}

#[allow(non_camel_case_types)]
pub type lkv_visit_fn = Option<
    unsafe extern "C" fn(
        context: *mut c_void,
        key: *const u8,
        key_len: usize,
        value: *const u8,
        value_len: usize,
    ) -> i32,
>;

thread_local! {
    static LAST_ERROR: RefCell<Vec<u8>> = RefCell::new(vec![0]);
}

struct FfiError {
    status: lkv_status,
    message: Option<String>,
}

type FfiResult<T> = Result<T, FfiError>;

impl FfiError {
    fn new(status: lkv_status, message: impl Into<String>) -> Self {
        Self {
            status,
            message: Some(message.into()),
        }
    }

    fn control(status: lkv_status) -> Self {
        Self {
            status,
            message: None,
        }
    }
}

impl From<Error> for FfiError {
    fn from(error: Error) -> Self {
        let status = match error {
            Error::Corrupted(_) => LKV_CORRUPTED,
            Error::DatabaseAlreadyOpen(_) => LKV_BUSY,
            Error::Unsupported(_) => LKV_UNSUPPORTED,
            Error::InvalidArgument(_) => LKV_INVALID_ARGUMENT,
            Error::DatabaseFull(_) => LKV_DATABASE_FULL,
            Error::MaintenanceRequired { .. } => LKV_MAINTENANCE_REQUIRED,
            Error::Poisoned => LKV_POISONED,
            Error::Io(_) => LKV_IO_ERROR,
        };
        Self::new(status, error.to_string())
    }
}

fn set_last_error(message: &str) {
    LAST_ERROR.with(|slot| {
        let mut slot = slot.borrow_mut();
        slot.clear();
        slot.reserve(message.len() + 1);
        for byte in message.bytes() {
            if byte == 0 {
                slot.extend_from_slice(b"\\0");
            } else {
                slot.push(byte);
            }
        }
        slot.push(0);
    });
}

fn clear_last_error() {
    LAST_ERROR.with(|slot| {
        let mut slot = slot.borrow_mut();
        slot.clear();
        slot.push(0);
    });
}

fn ffi_call(operation: impl FnOnce() -> FfiResult<()> + std::panic::UnwindSafe) -> lkv_status {
    clear_last_error();
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(Ok(())) => LKV_OK,
        Ok(Err(error)) => {
            if let Some(message) = error.message {
                set_last_error(&message);
            }
            error.status
        }
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("Rust panic crossed the lkv C API boundary");
            set_last_error(message);
            LKV_PANIC
        }
    }
}

unsafe fn required_ref<'a, T>(pointer: *const T, name: &str) -> FfiResult<&'a T> {
    if pointer.is_null() {
        Err(FfiError::new(
            LKV_INVALID_ARGUMENT,
            format!("{name} must not be NULL"),
        ))
    } else {
        // SAFETY: the C caller promises that non-null input pointers are valid
        // for the duration of this call.
        Ok(unsafe { &*pointer })
    }
}

unsafe fn required_mut<'a, T>(pointer: *mut T, name: &str) -> FfiResult<&'a mut T> {
    if pointer.is_null() {
        Err(FfiError::new(
            LKV_INVALID_ARGUMENT,
            format!("{name} must not be NULL"),
        ))
    } else {
        // SAFETY: the C caller promises exclusive access to mutable output and
        // handle pointers for the duration of this call.
        Ok(unsafe { &mut *pointer })
    }
}

unsafe fn input_bytes<'a>(pointer: *const u8, len: usize, name: &str) -> FfiResult<&'a [u8]> {
    if len == 0 {
        Ok(&[])
    } else if pointer.is_null() {
        Err(FfiError::new(
            LKV_INVALID_ARGUMENT,
            format!("{name} must not be NULL when its length is non-zero"),
        ))
    } else {
        // SAFETY: the C caller promises that `pointer` addresses `len` readable
        // bytes for the duration of this call.
        Ok(unsafe { slice::from_raw_parts(pointer, len) })
    }
}

fn read_database(database: &lkv_database) -> FfiResult<RwLockReadGuard<'_, Database>> {
    database
        .inner
        .read()
        .map_err(|_| FfiError::new(LKV_PANIC, "database lock was poisoned by a previous panic"))
}

fn write_database(database: &lkv_database) -> FfiResult<RwLockWriteGuard<'_, Database>> {
    database
        .inner
        .write()
        .map_err(|_| FfiError::new(LKV_PANIC, "database lock was poisoned by a previous panic"))
}

fn decode_options(options: Option<&lkv_options>) -> FfiResult<DatabaseOptions> {
    let Some(options) = options else {
        return Ok(DatabaseOptions::default());
    };
    let verification = match options.verification {
        LKV_VERIFICATION_ON_READ => VerificationMode::OnRead,
        LKV_VERIFICATION_FULL => VerificationMode::Full,
        value => {
            return Err(FfiError::new(
                LKV_INVALID_ARGUMENT,
                format!("unknown verification mode {value}"),
            ));
        }
    };
    Ok(DatabaseOptions::default()
        .with_verification(verification)
        .with_overlay_memory_limit(options.overlay_memory_limit)
        .with_max_database_bytes(options.max_database_bytes))
}

fn borrow_value(value: Option<&[u8]>, output: &mut *const u8, len: &mut usize) -> FfiResult<()> {
    *output = ptr::null();
    *len = 0;
    let Some(value) = value else {
        return Err(FfiError::control(LKV_NOT_FOUND));
    };
    *len = value.len();
    if !value.is_empty() {
        *output = value.as_ptr();
    }
    Ok(())
}

unsafe fn copy_value(
    value: Option<&[u8]>,
    output: *mut u8,
    capacity: usize,
    len: &mut usize,
) -> FfiResult<()> {
    *len = 0;
    if output.is_null() && capacity != 0 {
        return Err(FfiError::new(
            LKV_INVALID_ARGUMENT,
            "value must not be NULL when value_capacity is non-zero",
        ));
    }
    let Some(value) = value else {
        return Err(FfiError::control(LKV_NOT_FOUND));
    };
    *len = value.len();
    if capacity < value.len() {
        return Err(FfiError::control(LKV_BUFFER_TOO_SMALL));
    }
    if value.is_empty() {
        return Ok(());
    }
    debug_assert!(!output.is_null());
    // SAFETY: the C caller promises that `output` addresses `capacity`
    // writable bytes and does not overlap the database-owned value.
    unsafe { ptr::copy_nonoverlapping(value.as_ptr(), output, value.len()) };
    Ok(())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_version() -> u32 {
    1
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_last_error_message() -> *const c_char {
    LAST_ERROR.with(|slot| slot.borrow().as_ptr().cast())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_options_init(output: *mut lkv_options) -> lkv_status {
    ffi_call(|| {
        // SAFETY: validated by `required_mut`.
        let output = unsafe { required_mut(output, "output")? };
        let defaults = DatabaseOptions::default();
        *output = lkv_options {
            verification: LKV_VERIFICATION_ON_READ,
            overlay_memory_limit: defaults.overlay_memory_limit,
            max_database_bytes: defaults.max_database_bytes,
        };
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_database_open(
    path: *const c_char,
    options: *const lkv_options,
    output: *mut *mut lkv_database,
) -> lkv_status {
    ffi_call(|| {
        // SAFETY: all pointers are validated before dereferencing.
        let output = unsafe { required_mut(output, "output")? };
        *output = ptr::null_mut();
        // SAFETY: validated by `required_ref`, and C strings are required to be
        // NUL terminated by the API contract.
        let path = unsafe { required_ref(path, "path")? };
        // SAFETY: `path` came from a non-null C string pointer.
        let path = unsafe { CStr::from_ptr(path) }
            .to_str()
            .map_err(|_| FfiError::new(LKV_INVALID_ARGUMENT, "path must be valid UTF-8"))?;
        let options = if options.is_null() {
            None
        } else {
            // SAFETY: after verifying an exact ABI version, a non-null options
            // pointer addresses the layout declared by the matching header.
            Some(unsafe { &*options })
        };
        let database = Database::open_with_options(path, decode_options(options)?)?;
        *output = Box::into_raw(Box::new(lkv_database {
            inner: RwLock::new(database),
        }));
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_database_create(
    path: *const c_char,
    options: *const lkv_options,
    output: *mut *mut lkv_database,
) -> lkv_status {
    ffi_call(|| {
        // SAFETY: all pointers are validated before dereferencing.
        let output = unsafe { required_mut(output, "output")? };
        *output = ptr::null_mut();
        let path = unsafe { required_ref(path, "path")? };
        // SAFETY: `path` came from a non-null C string pointer.
        let path = unsafe { CStr::from_ptr(path) }
            .to_str()
            .map_err(|_| FfiError::new(LKV_INVALID_ARGUMENT, "path must be valid UTF-8"))?;
        let options = if options.is_null() {
            None
        } else {
            // SAFETY: the caller uses the exact ABI version declared by the
            // matching header and keeps the options alive for this call.
            Some(unsafe { &*options })
        };
        let database = Database::create_with_options(path, decode_options(options)?)?;
        *output = Box::into_raw(Box::new(lkv_database {
            inner: RwLock::new(database),
        }));
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_database_open_memory(
    options: *const lkv_options,
    output: *mut *mut lkv_database,
) -> lkv_status {
    ffi_call(|| {
        // SAFETY: pointers are validated before dereferencing.
        let output = unsafe { required_mut(output, "output")? };
        *output = ptr::null_mut();
        let options = if options.is_null() {
            None
        } else {
            // SAFETY: the caller uses the exact ABI version declared by the
            // matching header and keeps the options alive for this call.
            Some(unsafe { &*options })
        };
        let database = Database::memory_with_options(decode_options(options)?)?;
        *output = Box::into_raw(Box::new(lkv_database {
            inner: RwLock::new(database),
        }));
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_database_close(database: *mut lkv_database) -> lkv_status {
    ffi_call(|| {
        if !database.is_null() {
            // SAFETY: ownership of a database handle returned by this library
            // is transferred back exactly once by the C caller.
            drop(unsafe { Box::from_raw(database) });
        }
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_database_get(
    database: *const lkv_database,
    key: *const u8,
    key_len: usize,
    value: *mut u8,
    value_capacity: usize,
    value_len: *mut usize,
) -> lkv_status {
    ffi_call(|| {
        // SAFETY: input and output pointers are checked before use.
        let database = unsafe { required_ref(database, "database")? };
        let key = unsafe { input_bytes(key, key_len, "key")? };
        let value_len = unsafe { required_mut(value_len, "value_len")? };
        let database = read_database(database)?;
        // SAFETY: the caller supplies a writable output region as documented
        // by the C API. `copy_value` validates the nullable cases.
        unsafe { copy_value(database.get(key)?, value, value_capacity, value_len) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_database_get_ref(
    database: *const lkv_database,
    key: *const u8,
    key_len: usize,
    value: *mut *const u8,
    value_len: *mut usize,
) -> lkv_status {
    ffi_call(|| {
        // SAFETY: input and output pointers are checked before use.
        let database = unsafe { required_ref(database, "database")? };
        let key = unsafe { input_bytes(key, key_len, "key")? };
        let value = unsafe { required_mut(value, "value")? };
        let value_len = unsafe { required_mut(value_len, "value_len")? };
        let database = read_database(database)?;
        borrow_value(database.get(key)?, value, value_len)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_database_put(
    database: *const lkv_database,
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
) -> lkv_status {
    ffi_call(|| {
        // SAFETY: input pointers are checked before use.
        let database = unsafe { required_ref(database, "database")? };
        let key = unsafe { input_bytes(key, key_len, "key")? };
        let value = unsafe { input_bytes(value, value_len, "value")? };
        let mut database = write_database(database)?;
        let mut transaction = database.begin_write()?;
        transaction.put(key, value)?;
        transaction.commit()?;
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_database_delete(
    database: *const lkv_database,
    key: *const u8,
    key_len: usize,
) -> lkv_status {
    ffi_call(|| {
        // SAFETY: input pointers are checked before use.
        let database = unsafe { required_ref(database, "database")? };
        let key = unsafe { input_bytes(key, key_len, "key")? };
        let mut database = write_database(database)?;
        let mut transaction = database.begin_write()?;
        transaction.delete(key)?;
        transaction.commit()?;
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_database_len(
    database: *const lkv_database,
    output: *mut usize,
) -> lkv_status {
    ffi_call(|| {
        // SAFETY: pointers are checked before use.
        let database = unsafe { required_ref(database, "database")? };
        let output = unsafe { required_mut(output, "output")? };
        *output = read_database(database)?.len()?;
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_database_overlay_memory_usage(
    database: *const lkv_database,
    output: *mut usize,
) -> lkv_status {
    ffi_call(|| {
        // SAFETY: pointers are checked before use.
        let database = unsafe { required_ref(database, "database")? };
        let output = unsafe { required_mut(output, "output")? };
        *output = read_database(database)?.overlay_memory_usage();
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_database_get_stats(
    database: *const lkv_database,
    output: *mut lkv_database_stats,
) -> lkv_status {
    ffi_call(|| {
        // SAFETY: pointers are checked before use.
        let database = unsafe { required_ref(database, "database")? };
        let output = unsafe { required_mut(output, "output")? };
        let stats = read_database(database)?.stats()?;
        *output = lkv_database_stats {
            storage_bytes: stats.storage_bytes,
            base_bytes: stats.base_bytes,
            base_entries: stats.base_entries,
            overlay_entries: stats.overlay_entries,
            overlay_log_bytes: stats.overlay_log_bytes,
            overlay_memory_bytes: stats.overlay_memory_bytes,
            stale_bytes: stats.stale_bytes,
            generation: stats.generation,
        };
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_database_sync(database: *const lkv_database) -> lkv_status {
    ffi_call(|| {
        // SAFETY: pointer is checked before use.
        let database = unsafe { required_ref(database, "database")? };
        read_database(database)?.sync()?;
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_database_verify(database: *const lkv_database) -> lkv_status {
    ffi_call(|| {
        // SAFETY: pointer is checked before use.
        let database = unsafe { required_ref(database, "database")? };
        read_database(database)?.verify()?;
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_database_compact(database: *const lkv_database) -> lkv_status {
    ffi_call(|| {
        // SAFETY: pointer is checked before use.
        let database = unsafe { required_ref(database, "database")? };
        write_database(database)?.compact()?;
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_snapshot_create(
    database: *const lkv_database,
    output: *mut *mut lkv_snapshot,
) -> lkv_status {
    ffi_call(|| {
        // SAFETY: pointers are checked before use.
        let database = unsafe { required_ref(database, "database")? };
        let output = unsafe { required_mut(output, "output")? };
        *output = ptr::null_mut();
        let snapshot = read_database(database)?.snapshot()?;
        *output = Box::into_raw(Box::new(lkv_snapshot { inner: snapshot }));
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_snapshot_close(snapshot: *mut lkv_snapshot) -> lkv_status {
    ffi_call(|| {
        if !snapshot.is_null() {
            // SAFETY: ownership is transferred back exactly once by the caller.
            drop(unsafe { Box::from_raw(snapshot) });
        }
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_snapshot_get(
    snapshot: *const lkv_snapshot,
    key: *const u8,
    key_len: usize,
    value: *mut u8,
    value_capacity: usize,
    value_len: *mut usize,
) -> lkv_status {
    ffi_call(|| {
        // SAFETY: pointers are checked before use.
        let snapshot = unsafe { required_ref(snapshot, "snapshot")? };
        let key = unsafe { input_bytes(key, key_len, "key")? };
        let value_len = unsafe { required_mut(value_len, "value_len")? };
        // SAFETY: the caller supplies a writable output region as documented
        // by the C API. `copy_value` validates the nullable cases.
        unsafe { copy_value(snapshot.inner.get(key)?, value, value_capacity, value_len) }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_snapshot_get_ref(
    snapshot: *const lkv_snapshot,
    key: *const u8,
    key_len: usize,
    value: *mut *const u8,
    value_len: *mut usize,
) -> lkv_status {
    ffi_call(|| {
        // SAFETY: pointers are checked before use.
        let snapshot = unsafe { required_ref(snapshot, "snapshot")? };
        let key = unsafe { input_bytes(key, key_len, "key")? };
        let value = unsafe { required_mut(value, "value")? };
        let value_len = unsafe { required_mut(value_len, "value_len")? };
        borrow_value(snapshot.inner.get(key)?, value, value_len)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_snapshot_visit(
    snapshot: *const lkv_snapshot,
    visitor: lkv_visit_fn,
    context: *mut c_void,
) -> lkv_status {
    ffi_call(|| {
        // SAFETY: snapshot is checked, and the callback contract is defined by
        // the public C header.
        let snapshot = unsafe { required_ref(snapshot, "snapshot")? };
        let visitor = visitor
            .ok_or_else(|| FfiError::new(LKV_INVALID_ARGUMENT, "visitor must not be NULL"))?;
        for entry in snapshot.inner.iter()? {
            let (key, value) = entry?;
            // SAFETY: slices remain valid for this synchronous callback only.
            if unsafe {
                visitor(
                    context,
                    key.as_ptr(),
                    key.len(),
                    value.as_ptr(),
                    value.len(),
                )
            } == 0
            {
                break;
            }
        }
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_database_visit(
    database: *const lkv_database,
    visitor: lkv_visit_fn,
    context: *mut c_void,
) -> lkv_status {
    ffi_call(|| {
        // SAFETY: database is checked, and the callback contract is defined by
        // the public C header.
        let database = unsafe { required_ref(database, "database")? };
        let visitor = visitor
            .ok_or_else(|| FfiError::new(LKV_INVALID_ARGUMENT, "visitor must not be NULL"))?;
        let database = read_database(database)?;
        for entry in database.iter()? {
            let (key, value) = entry?;
            // SAFETY: slices remain valid for this synchronous callback only.
            if unsafe {
                visitor(
                    context,
                    key.as_ptr(),
                    key.len(),
                    value.as_ptr(),
                    value.len(),
                )
            } == 0
            {
                break;
            }
        }
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_write_batch_create(output: *mut *mut lkv_write_batch) -> lkv_status {
    ffi_call(|| {
        // SAFETY: output is checked before use.
        let output = unsafe { required_mut(output, "output")? };
        *output = Box::into_raw(Box::new(lkv_write_batch {
            entries: HashMap::new(),
        }));
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_write_batch_close(batch: *mut lkv_write_batch) -> lkv_status {
    ffi_call(|| {
        if !batch.is_null() {
            // SAFETY: ownership is transferred back exactly once by the caller.
            drop(unsafe { Box::from_raw(batch) });
        }
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_write_batch_clear(batch: *mut lkv_write_batch) -> lkv_status {
    ffi_call(|| {
        // SAFETY: batch is checked before use.
        unsafe { required_mut(batch, "batch")? }.entries.clear();
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_write_batch_len(
    batch: *const lkv_write_batch,
    output: *mut usize,
) -> lkv_status {
    ffi_call(|| {
        // SAFETY: pointers are checked before use.
        let batch = unsafe { required_ref(batch, "batch")? };
        *unsafe { required_mut(output, "output")? } = batch.entries.len();
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_write_batch_put(
    batch: *mut lkv_write_batch,
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
) -> lkv_status {
    ffi_call(|| {
        // SAFETY: pointers are checked before use.
        let batch = unsafe { required_mut(batch, "batch")? };
        let key = unsafe { input_bytes(key, key_len, "key")? };
        let value = unsafe { input_bytes(value, value_len, "value")? };
        batch.entries.insert(key.to_vec(), Some(value.to_vec()));
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_write_batch_delete(
    batch: *mut lkv_write_batch,
    key: *const u8,
    key_len: usize,
) -> lkv_status {
    ffi_call(|| {
        // SAFETY: pointers are checked before use.
        let batch = unsafe { required_mut(batch, "batch")? };
        let key = unsafe { input_bytes(key, key_len, "key")? };
        batch.entries.insert(key.to_vec(), None);
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lkv_database_commit_write_batch(
    database: *const lkv_database,
    batch: *mut lkv_write_batch,
) -> lkv_status {
    ffi_call(|| {
        // SAFETY: pointers are checked before use.
        let database = unsafe { required_ref(database, "database")? };
        let batch = unsafe { required_mut(batch, "batch")? };
        let mut database = write_database(database)?;
        let mut transaction = database.begin_write()?;
        for (key, value) in &batch.entries {
            if let Some(value) = value {
                transaction.put(key, value)?;
            } else {
                transaction.delete(key)?;
            }
        }
        transaction.commit()?;
        batch.entries.clear();
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

    fn temp_path() -> CString {
        let path = std::env::temp_dir().join(format!(
            "lkv-c-test-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed)
        ));
        CString::new(path.to_str().unwrap()).unwrap()
    }

    fn remove_database(path: &CStr) {
        fs::remove_file(path.to_str().unwrap()).unwrap();
    }

    unsafe extern "C" fn count_entry(
        context: *mut c_void,
        _key: *const u8,
        _key_len: usize,
        _value: *const u8,
        _value_len: usize,
    ) -> i32 {
        // SAFETY: the test passes a live `usize` as callback context.
        unsafe { *(context.cast::<usize>()) += 1 };
        1
    }

    #[test]
    fn c_api_round_trip_batch_snapshot_and_maintenance() {
        // SAFETY: every pointer passed below comes from a live Rust value or an
        // opaque handle returned by the C API and is closed exactly once.
        unsafe {
            let path = temp_path();
            let mut database = ptr::null_mut();
            assert_eq!(
                lkv_database_create(path.as_ptr(), ptr::null(), &mut database),
                LKV_OK
            );
            assert!(!database.is_null());

            assert_eq!(
                lkv_database_put(database, b"key".as_ptr(), 3, b"old".as_ptr(), 3),
                LKV_OK
            );
            assert_eq!(
                lkv_database_put(database, b"gone".as_ptr(), 4, b"soon".as_ptr(), 4),
                LKV_OK
            );
            let mut snapshot = ptr::null_mut();
            assert_eq!(lkv_snapshot_create(database, &mut snapshot), LKV_OK);
            let mut snapshot_count = 0usize;
            assert_eq!(
                lkv_snapshot_visit(
                    snapshot,
                    Some(count_entry),
                    (&mut snapshot_count as *mut usize).cast()
                ),
                LKV_OK
            );
            assert_eq!(snapshot_count, 2);

            let mut batch = ptr::null_mut();
            assert_eq!(lkv_write_batch_create(&mut batch), LKV_OK);
            assert_eq!(
                lkv_write_batch_put(batch, b"key".as_ptr(), 3, b"new".as_ptr(), 3),
                LKV_OK
            );
            assert_eq!(
                lkv_write_batch_put(batch, b"two".as_ptr(), 3, b"value".as_ptr(), 5),
                LKV_OK
            );
            assert_eq!(lkv_write_batch_delete(batch, b"gone".as_ptr(), 4), LKV_OK);
            assert_eq!(lkv_database_commit_write_batch(database, batch), LKV_OK);

            let mut count = 0usize;
            assert_eq!(
                lkv_database_visit(
                    database,
                    Some(count_entry),
                    (&mut count as *mut usize).cast()
                ),
                LKV_OK
            );
            assert_eq!(count, 2);

            let mut stats: lkv_database_stats = std::mem::zeroed();
            assert_eq!(lkv_database_get_stats(database, &mut stats), LKV_OK);
            assert_eq!(stats.base_entries, 0);
            assert_eq!(stats.overlay_entries, 3);
            assert!(stats.overlay_log_bytes > 0);

            let mut value = ptr::null();
            let mut value_len = 0;
            assert_eq!(
                lkv_database_get_ref(database, b"two".as_ptr(), 3, &mut value, &mut value_len),
                LKV_OK
            );
            assert_eq!(slice::from_raw_parts(value, value_len), b"value");

            let mut old = ptr::null();
            let mut old_len = 0;
            assert_eq!(
                lkv_snapshot_get_ref(snapshot, b"key".as_ptr(), 3, &mut old, &mut old_len),
                LKV_OK
            );
            assert_eq!(slice::from_raw_parts(old, old_len), b"old");
            assert_eq!(lkv_database_compact(database), LKV_BUSY);
            assert_eq!(slice::from_raw_parts(old, old_len), b"old");
            assert_eq!(lkv_snapshot_close(snapshot), LKV_OK);
            assert_eq!(lkv_database_compact(database), LKV_OK);
            assert_eq!(lkv_database_get_stats(database, &mut stats), LKV_OK);
            assert_eq!(stats.base_entries, 2);
            assert_eq!(stats.overlay_entries, 0);
            assert_eq!(stats.stale_bytes, 0);
            assert_eq!(lkv_database_verify(database), LKV_OK);

            assert_eq!(lkv_write_batch_close(batch), LKV_OK);
            assert_eq!(lkv_database_close(database), LKV_OK);
            remove_database(&path);
        }
    }

    #[test]
    fn null_arguments_and_not_found_are_status_codes() {
        // SAFETY: valid pointers obey the C header contract; the deliberate
        // NULL case is an API validation test.
        unsafe {
            let mut options = lkv_options {
                verification: 99,
                overlay_memory_limit: 0,
                max_database_bytes: 0,
            };
            assert_eq!(lkv_options_init(&mut options), LKV_OK);
            assert_eq!(options.verification, LKV_VERIFICATION_ON_READ);
            assert_eq!(lkv_options_init(ptr::null_mut()), LKV_INVALID_ARGUMENT);
            assert!(!lkv_last_error_message().is_null());
            assert!(
                !CStr::from_ptr(lkv_last_error_message())
                    .to_bytes()
                    .is_empty()
            );
            assert_eq!(lkv_options_init(&mut options), LKV_OK);
            assert!(
                CStr::from_ptr(lkv_last_error_message())
                    .to_bytes()
                    .is_empty()
            );

            let path = temp_path();
            let mut database = ptr::null_mut();
            options.verification = 99;
            assert_eq!(
                lkv_database_open(path.as_ptr(), &options, &mut database),
                LKV_INVALID_ARGUMENT
            );
            assert!(database.is_null());
            assert_eq!(
                lkv_database_open(path.as_ptr(), ptr::null(), &mut database),
                LKV_IO_ERROR
            );
            assert!(database.is_null());
            assert_eq!(
                lkv_database_create(path.as_ptr(), ptr::null(), &mut database),
                LKV_OK
            );
            let mut value = ptr::dangling();
            let mut len = usize::MAX;
            assert_eq!(
                lkv_database_get_ref(database, b"missing".as_ptr(), 7, &mut value, &mut len),
                LKV_NOT_FOUND
            );
            assert!(value.is_null());
            assert_eq!(len, 0);
            assert_eq!(lkv_database_close(database), LKV_OK);
            remove_database(&path);
        }
    }

    #[test]
    fn memory_database_uses_the_same_c_api() {
        // SAFETY: the handle is returned by the C API and closed exactly once.
        unsafe {
            let mut database = ptr::null_mut();
            assert_eq!(lkv_database_open_memory(ptr::null(), &mut database), LKV_OK);
            assert_eq!(
                lkv_database_put(database, b"key".as_ptr(), 3, b"value".as_ptr(), 5),
                LKV_OK
            );
            assert_eq!(lkv_database_sync(database), LKV_OK);
            assert_eq!(lkv_database_compact(database), LKV_OK);
            let mut value = ptr::null();
            let mut len = 0;
            assert_eq!(
                lkv_database_get_ref(database, b"key".as_ptr(), 3, &mut value, &mut len,),
                LKV_OK
            );
            assert_eq!(slice::from_raw_parts(value, len), b"value");
            assert_eq!(lkv_database_close(database), LKV_OK);
        }
    }

    #[test]
    fn zero_copy_gets_handle_missing_and_empty_values() {
        // SAFETY: handles and pointers obey the C header contract and are closed once.
        unsafe {
            let mut database = ptr::null_mut();
            assert_eq!(lkv_database_open_memory(ptr::null(), &mut database), LKV_OK);
            assert_eq!(
                lkv_database_put(database, b"empty".as_ptr(), 5, ptr::null(), 0),
                LKV_OK
            );

            let mut database_value = ptr::dangling();
            let mut database_value_len = usize::MAX;
            assert_eq!(
                lkv_database_get_ref(
                    database,
                    b"empty".as_ptr(),
                    5,
                    &mut database_value,
                    &mut database_value_len,
                ),
                LKV_OK
            );
            assert!(database_value.is_null());
            assert_eq!(database_value_len, 0);

            let mut snapshot = ptr::null_mut();
            assert_eq!(lkv_snapshot_create(database, &mut snapshot), LKV_OK);

            let mut value = ptr::dangling();
            let mut value_len = usize::MAX;
            assert_eq!(
                lkv_snapshot_get_ref(snapshot, b"missing".as_ptr(), 7, &mut value, &mut value_len,),
                LKV_NOT_FOUND
            );
            assert!(value.is_null());
            assert_eq!(value_len, 0);

            value = ptr::dangling();
            value_len = usize::MAX;
            assert_eq!(
                lkv_snapshot_get_ref(snapshot, b"empty".as_ptr(), 5, &mut value, &mut value_len,),
                LKV_OK
            );
            assert!(value.is_null());
            assert_eq!(value_len, 0);

            assert_eq!(lkv_snapshot_close(snapshot), LKV_OK);
            assert_eq!(lkv_database_close(database), LKV_OK);
        }
    }

    #[test]
    fn copying_gets_report_required_size_without_partial_output() {
        // SAFETY: handles and buffers remain live for every call and are closed once.
        unsafe {
            let mut database = ptr::null_mut();
            assert_eq!(lkv_database_open_memory(ptr::null(), &mut database), LKV_OK);
            assert_eq!(
                lkv_database_put(database, b"key".as_ptr(), 3, b"value".as_ptr(), 5),
                LKV_OK
            );
            assert_eq!(
                lkv_database_put(database, b"empty".as_ptr(), 5, ptr::null(), 0),
                LKV_OK
            );

            let mut short = [0xa5; 3];
            let mut len = 0;
            assert_eq!(
                lkv_database_get(
                    database,
                    b"key".as_ptr(),
                    3,
                    short.as_mut_ptr(),
                    short.len(),
                    &mut len,
                ),
                LKV_BUFFER_TOO_SMALL
            );
            assert_eq!(len, 5);
            assert_eq!(short, [0xa5; 3]);

            len = 0;
            assert_eq!(
                lkv_database_get(database, b"key".as_ptr(), 3, ptr::null_mut(), 0, &mut len,),
                LKV_BUFFER_TOO_SMALL
            );
            assert_eq!(len, 5);

            let mut value = [0; 5];
            assert_eq!(
                lkv_database_get(
                    database,
                    b"key".as_ptr(),
                    3,
                    value.as_mut_ptr(),
                    value.len(),
                    &mut len,
                ),
                LKV_OK
            );
            assert_eq!(len, value.len());
            assert_eq!(&value, b"value");

            len = usize::MAX;
            assert_eq!(
                lkv_database_get(
                    database,
                    b"missing".as_ptr(),
                    7,
                    ptr::null_mut(),
                    0,
                    &mut len,
                ),
                LKV_NOT_FOUND
            );
            assert_eq!(len, 0);
            assert_eq!(
                lkv_database_get(database, b"empty".as_ptr(), 5, ptr::null_mut(), 0, &mut len,),
                LKV_OK
            );
            assert_eq!(len, 0);

            let mut snapshot = ptr::null_mut();
            assert_eq!(lkv_snapshot_create(database, &mut snapshot), LKV_OK);
            value.fill(0);
            assert_eq!(
                lkv_snapshot_get(
                    snapshot,
                    b"key".as_ptr(),
                    3,
                    value.as_mut_ptr(),
                    value.len(),
                    &mut len,
                ),
                LKV_OK
            );
            assert_eq!(&value, b"value");

            assert_eq!(lkv_snapshot_close(snapshot), LKV_OK);
            assert_eq!(lkv_database_close(database), LKV_OK);
        }
    }

    #[test]
    fn database_handle_supports_parallel_reads() {
        // SAFETY: the database remains live until every reader has joined.
        unsafe {
            let mut database = ptr::null_mut();
            assert_eq!(lkv_database_open_memory(ptr::null(), &mut database), LKV_OK);
            assert_eq!(
                lkv_database_put(database, b"key".as_ptr(), 3, b"value".as_ptr(), 5),
                LKV_OK
            );
            let address = database as usize;
            let readers: Vec<_> = (0..4)
                .map(|_| {
                    std::thread::spawn(move || {
                        let database = address as *const lkv_database;
                        let mut value = ptr::null();
                        let mut value_len = 0;
                        for _ in 0..1_000 {
                            // SAFETY: output slots are thread-local and no writer runs.
                            assert_eq!(
                                lkv_database_get_ref(
                                    database,
                                    b"key".as_ptr(),
                                    3,
                                    &mut value,
                                    &mut value_len,
                                ),
                                LKV_OK
                            );
                        }
                        assert_eq!(slice::from_raw_parts(value, value_len), b"value");
                    })
                })
                .collect();
            for reader in readers {
                reader.join().unwrap();
            }
            assert_eq!(lkv_database_close(database), LKV_OK);
        }
    }

    #[test]
    fn c_api_create_refuses_an_existing_database() {
        // SAFETY: handles returned by successful calls are closed exactly once.
        unsafe {
            let path = temp_path();
            let mut database = ptr::null_mut();
            assert_eq!(
                lkv_database_create(path.as_ptr(), ptr::null(), &mut database),
                LKV_OK
            );
            assert!(!database.is_null());

            let mut duplicate = ptr::null_mut();
            assert_eq!(
                lkv_database_create(path.as_ptr(), ptr::null(), &mut duplicate),
                LKV_IO_ERROR
            );
            assert!(duplicate.is_null());
            assert_eq!(lkv_database_close(database), LKV_OK);

            let mut reopened = ptr::null_mut();
            assert_eq!(
                lkv_database_open(path.as_ptr(), ptr::null(), &mut reopened),
                LKV_OK
            );
            assert_eq!(lkv_database_close(reopened), LKV_OK);
            remove_database(&path);
        }
    }
}
