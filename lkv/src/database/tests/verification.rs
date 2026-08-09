use super::*;
use crate::VerificationMode;

#[test]
fn rejects_corrupt_base_lengths() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    db.put(b"key", b"value")?;
    db.compact()?;
    let record_offset = db.base.offset + BASE_HEADER as u64 + db.base.slots * SLOT_SIZE as u64;
    drop(db);
    let mut file = OpenOptions::new().write(true).open(&dir)?;
    file.seek(SeekFrom::Start(record_offset + 4))?;
    file.write_all(&u32::MAX.to_le_bytes())?;
    file.sync_all()?;
    let error = Database::open_with_options(
        &dir,
        DatabaseOptions {
            verification: VerificationMode::Full,
            ..DatabaseOptions::default()
        },
    )
    .err()
    .expect("corrupt base must fail open");
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn on_read_defers_bad_record_bounds_and_iteration_returns_error() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    db.put(b"key", b"value")?;
    db.compact()?;
    let record_offset = db.base.offset + BASE_HEADER as u64 + db.base.slots * SLOT_SIZE as u64;
    drop(db);

    let mut file = OpenOptions::new().write(true).open(&dir)?;
    file.seek(SeekFrom::Start(record_offset + 4))?;
    file.write_all(&u32::MAX.to_le_bytes())?;
    file.sync_all()?;
    drop(file);

    let db = Database::open(&dir)?;
    let mut entries = db.iter()?;
    let error = entries
        .next()
        .expect("corrupt record must produce one iterator error")
        .expect_err("corrupt record must not be returned");
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    assert!(entries.next().is_none());
    drop(entries);
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn semantic_verification_enables_only_the_trusted_iterator_path() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    db.put(b"key", b"value")?;
    db.compact()?;
    assert!(db.iter_raw().base_trusted);
    drop(db);

    let db = Database::open(&dir)?;
    assert!(!db.iter_raw().base_trusted);
    db.verify()?;
    assert!(db.iter_raw().base_trusted);
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn rejects_base_value_corruption() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    db.put(b"key", b"value")?;
    db.compact()?;
    let value_offset = db.base.offset
        + BASE_HEADER as u64
        + db.base.slots * SLOT_SIZE as u64
        + 8
        + b"key".len() as u64;
    drop(db);

    let path = dir.clone();
    let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
    file.seek(SeekFrom::Start(value_offset))?;
    let mut byte = [0];
    file.read_exact(&mut byte)?;
    byte[0] ^= 0xff;
    file.seek(SeekFrom::Start(value_offset))?;
    file.write_all(&byte)?;
    file.sync_all()?;
    let db = Database::open(&dir)?;
    let error = db
        .get(b"key")
        .expect_err("corrupt base block must fail on first read");
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn on_read_verifies_every_block_spanned_by_a_value() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    let value = vec![b'v'; CHECKSUM_BLOCK_SIZE * 2];
    db.put(b"large", &value)?;
    db.compact()?;
    let corrupt_offset = db.base.offset + CHECKSUM_BLOCK_SIZE as u64 + 123;
    drop(db);

    let path = dir.clone();
    let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
    file.seek(SeekFrom::Start(corrupt_offset))?;
    let mut byte = [0];
    file.read_exact(&mut byte)?;
    byte[0] ^= 0xff;
    file.seek(SeekFrom::Start(corrupt_offset))?;
    file.write_all(&byte)?;
    file.sync_all()?;
    drop(file);

    let db = Database::open(&dir)?;
    let error = db.get(b"large").expect_err("second block is corrupt");
    match error {
        Error::Corrupted(corruption) => {
            assert_eq!(corruption.kind(), CorruptionKind::BlockChecksum);
            assert_eq!(corruption.block_index(), Some(1));
        }
        other => panic!("unexpected error: {other}"),
    }
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn on_read_open_still_rejects_checksum_metadata_corruption() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    db.put(b"key", b"value")?;
    db.compact()?;
    let checksum_table_offset = db.base.offset + db.base.verifier.data_size() as u64;
    drop(db);

    let path = dir.clone();
    let mut file = OpenOptions::new().write(true).open(&path)?;
    file.seek(SeekFrom::Start(checksum_table_offset))?;
    file.write_all(&0u32.to_le_bytes())?;
    file.sync_all()?;
    let error = Database::open(&dir)
        .err()
        .expect("checksum table metadata must be protected");
    assert!(matches!(error, Error::Corrupted(_)));
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn map_segment_rejects_ranges_beyond_the_file() -> Result<()> {
    let dir = temp_dir();
    let db = Database::open(&dir)?;
    let file = db.storage.file().expect("persistent test database");
    let file_len = file.metadata()?.len();
    let error = segment::map(file, file_len, 1).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn unknown_format_version_is_reported_as_unsupported() -> Result<()> {
    let dir = temp_dir();
    let db = Database::open(&dir)?;
    drop(db);
    let mut file = OpenOptions::new().read(true).write(true).open(&dir)?;
    let mut newer = [0; SUPERBLOCK_SIZE as usize];
    file.seek(SeekFrom::Start(SUPERBLOCK_SIZE))?;
    file.read_exact(&mut newer)?;
    newer[8..12].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
    newer[16..24].copy_from_slice(&2u64.to_le_bytes());
    let checksum_offset = HEADER_SIZE - size_of::<u32>();
    let checksum = crc32c::crc32c(&newer[..checksum_offset]);
    newer[checksum_offset..HEADER_SIZE].copy_from_slice(&checksum.to_le_bytes());
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&newer)?;
    file.seek(SeekFrom::End(0))?;
    file.write_all(&[1, 2, 3])?;
    file.sync_all()?;
    let original_len = file.metadata()?.len();
    drop(file);

    let error = Database::open(&dir)
        .err()
        .expect("unknown version must be rejected");
    assert!(matches!(error, Error::Unsupported(_)));
    assert_eq!(fs::metadata(&dir)?.len(), original_len);
    remove_test_database(&dir)?;
    Ok(())
}
