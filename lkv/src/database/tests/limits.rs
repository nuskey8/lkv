use super::*;
use crate::format::log::{Marker, write_record_header};

#[test]
fn database_size_limit_rejects_append_before_mutation() -> Result<()> {
    let dir = temp_dir();
    let db = Database::open(&dir)?;
    let file_len = db.storage.len()?;
    drop(db);
    let mut db = Database::open_with_options(
        &dir,
        DatabaseOptions {
            max_database_bytes: file_len,
            ..DatabaseOptions::default()
        },
    )?;
    let before = db.storage.len()?;
    let error = db.put(b"key", b"value").unwrap_err();
    assert!(matches!(error, Error::DatabaseFull(_)));
    assert_eq!(db.storage.len()?, before);
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn memory_database_size_limit_rejects_append_before_mutation() -> Result<()> {
    let probe = Database::memory()?;
    let initial_len = probe.storage.len()?;
    drop(probe);
    let mut db = Database::memory_with_options(DatabaseOptions {
        max_database_bytes: initial_len,
        ..DatabaseOptions::default()
    })?;
    let error = db.put(b"key", b"value").unwrap_err();
    assert!(matches!(error, Error::DatabaseFull(_)));
    assert_eq!(db.storage.len()?, initial_len);
    assert!(db.is_empty()?);
    Ok(())
}

#[test]
fn overlay_limit_stops_writes_without_running_maintenance() -> Result<()> {
    let dir = temp_dir();
    let db = Database::open(&dir)?;
    let file_len = db.storage.len()?;
    drop(db);

    let mut db = Database::open_with_options(
        &dir,
        DatabaseOptions {
            overlay_memory_limit: 0,
            max_database_bytes: file_len + 64,
            ..DatabaseOptions::default()
        },
    )?;
    let mut transaction = db.begin_write()?;
    transaction.put(b"key", b"value")?;
    transaction.commit()?;
    assert_eq!(db.get(b"key")?, Some(b"value".as_slice()));
    let before = db.storage.len()?;
    let error = match db.begin_write() {
        Ok(_) => panic!("write must require maintenance"),
        Err(error) => error,
    };
    assert!(matches!(error, Error::MaintenanceRequired { .. }));
    assert_eq!(db.storage.len()?, before);
    assert_eq!(db.get(b"key")?, Some(b"value".as_slice()));
    assert!(matches!(db.compact(), Err(Error::DatabaseFull(_))));
    drop(db);

    let db = Database::open(&dir)?;
    assert_eq!(db.get(b"key")?, Some(b"value".as_slice()));
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn malformed_sizes_fail_without_panicking_or_allocating() -> Result<()> {
    let mut header = vec![0; BASE_HEADER];
    header[..4].copy_from_slice(b"HASH");
    header[4..8].copy_from_slice(&(BASE_HEADER as u32).to_le_bytes());
    header[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(
        read_base_header(&header).unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    header[8..16].copy_from_slice(&0u64.to_le_bytes());
    header[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
    let (slots, len) = read_base_header(&header)?;
    assert_eq!(
        validate_base(&header, DATA_START, slots, len)
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidData
    );
    Ok(())
}

#[test]
fn maximal_truncated_overlay_record_is_discarded_before_allocation() -> Result<()> {
    let dir = temp_dir();
    let db = Database::open(&dir)?;
    let offset = db.storage.len()?;
    drop(db);

    let path = dir.clone();
    let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
    let payload_len = u32::MAX;
    file.seek(SeekFrom::Start(offset))?;
    write_record_header(&mut file, Marker::Batch, 0, payload_len, 0)?;
    file.sync_all()?;
    drop(file);

    let db = Database::open(&dir)?;
    assert_eq!(db.storage.len()?, offset);
    assert!(db.is_empty()?);
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn public_writes_enforce_allocation_limits_before_file_mutation() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    let before = db.storage.len()?;
    let oversized_key = vec![0; MAX_KEY_SIZE + 1];
    let mut transaction = db.begin_write()?;
    let error = transaction
        .put(&oversized_key, b"value")
        .expect_err("oversized key must be rejected");
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    transaction.abort();
    assert_eq!(db.storage.len()?, before);
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn reserved_value_that_cannot_fit_a_batch_is_rejected_before_allocation() -> Result<()> {
    let mut db = Database::memory()?;
    let mut transaction = db.begin_write()?;
    let error = transaction
        .put_reserved(b"key", u32::MAX as usize, |_| {
            panic!("oversized reservation callback must not run")
        })
        .expect_err("oversized reservation must fail");
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(transaction.is_empty());
    Ok(())
}
