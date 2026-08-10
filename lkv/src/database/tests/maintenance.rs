use super::*;

fn assert_memory_base_is_shared(database: &Database) {
    let BaseBytes::Memory { bytes, .. } = &database.base.mapping else {
        panic!("memory database Base must use memory bytes");
    };
    let storage_bytes = database
        .storage
        .memory_base_mapping()
        .expect("memory storage must retain the active Base");
    assert!(Arc::ptr_eq(bytes, storage_bytes));
}

#[test]
fn memory_storage_shares_base_while_overlay_grows() -> Result<()> {
    let mut db = Database::memory()?;
    assert_memory_base_is_shared(&db);
    let materialized = db.storage.memory_materialized_bytes().unwrap();
    let initial = match &db.base.mapping {
        BaseBytes::Memory { bytes, .. } => Arc::clone(bytes),
        BaseBytes::Mapped(_) => unreachable!(),
    };

    db.put(b"overlay", b"value")?;

    let current = match &db.base.mapping {
        BaseBytes::Memory { bytes, .. } => bytes,
        BaseBytes::Mapped(_) => unreachable!(),
    };
    assert!(Arc::ptr_eq(&initial, current));
    assert_memory_base_is_shared(&db);
    assert_eq!(db.storage.memory_materialized_bytes(), Some(materialized));
    assert!(db.stats()?.storage_bytes as usize > materialized);
    Ok(())
}

#[test]
fn compact_rewrites_and_shrinks_the_single_file() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    {
        let mut txn = db.begin_write()?;
        for i in 0..1_000u32 {
            txn.put(i.to_le_bytes(), [b'x'; 100])?;
        }
        txn.commit()?;
    }
    db.compact()?;
    let first_compacted = fs::metadata(&dir)?.len();
    {
        let mut txn = db.begin_write()?;
        for i in 0..1_000u32 {
            txn.put(i.to_le_bytes(), [b'y'; 100])?;
        }
        txn.commit()?;
    }
    let before = fs::metadata(&dir)?.len();
    db.compact()?;
    let after = fs::metadata(&dir)?.len();
    assert!(after < before);
    assert_eq!(after, first_compacted);
    assert_eq!(db.get(500u32.to_le_bytes())?, Some(&[b'y'; 100][..]));
    assert_eq!(fs::read_dir(dir.parent().unwrap())?.count(), 1);
    drop(db);
    let db = Database::open(&dir)?;
    assert_eq!(db.len()?, 1_000);
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn compact_leaves_redundant_superblocks() -> Result<()> {
    for damaged_offset in [0, SUPERBLOCK_SIZE] {
        let dir = temp_dir();
        let mut db = Database::open(&dir)?;
        db.put(b"key", b"value")?;
        db.compact()?;
        drop(db);

        let mut file = OpenOptions::new().read(true).write(true).open(&dir)?;
        file.seek(SeekFrom::Start(damaged_offset))?;
        file.write_all(&[0; HEADER_SIZE])?;
        file.sync_all()?;
        drop(file);

        let db = Database::open(&dir)?;
        assert_eq!(db.get(b"key")?, Some(b"value".as_slice()));
        drop(db);
        remove_test_database(&dir)?;
    }
    Ok(())
}

#[test]
fn compact_requires_snapshots_to_be_dropped() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    db.put(b"key", b"value")?;
    let snapshot = db.snapshot()?;

    let error = db
        .compact()
        .expect_err("a live snapshot must prevent compaction");
    assert_eq!(error.kind(), ErrorKind::WouldBlock);
    assert_eq!(db.get(b"key")?, Some(b"value".as_slice()));

    drop(snapshot);
    db.compact()?;
    assert_eq!(db.get(b"key")?, Some(b"value".as_slice()));
    assert!(matches!(&db.base.mapping, BaseBytes::Mapped(_)));
    assert!(!db.storage.is_memory());
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn compact_rejects_snapshots_before_verifying_the_base() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    let snapshot = db.snapshot()?;
    let checksum = db.base.checksum;
    db.base.checksum ^= 1;

    let error = db
        .compact()
        .expect_err("a live snapshot must be rejected before verification");
    assert_eq!(error.kind(), ErrorKind::WouldBlock);

    db.base.checksum = checksum;
    drop(snapshot);
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn compact_is_a_no_op_for_an_already_compact_database() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    db.put(b"key", b"value")?;
    db.compact()?;
    let generation = db.base.generation;
    let storage_len = db.storage.len()?;

    db.compact()?;

    assert_eq!(db.base.generation, generation);
    assert_eq!(db.storage.len()?, storage_len);
    assert_eq!(db.get(b"key")?, Some(b"value".as_slice()));
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn mappings_exclude_metadata_and_overlay() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    db.put(b"stable", b"old")?;
    db.compact()?;
    let snapshot = db.snapshot()?;
    db.put(b"overlay-only", b"new")?;
    assert!(db.storage.len()? > db.base.mapping.len() as u64);
    drop(snapshot);
    db.compact()?;

    assert_eq!(db.get(b"overlay-only")?, Some(b"new".as_slice()));
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn failed_published_base_install_poisons_the_handle() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    let injected = Error::from_io(ErrorKind::Other, "injected base install failure");

    let error = db
        .finish_superblock_install(Err(injected))
        .expect_err("published base install must fail");
    assert_eq!(error.kind(), ErrorKind::Other);
    assert_eq!(db.len()?, 0);
    assert!(matches!(db.begin_write(), Err(Error::Poisoned)));

    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn stats_describe_active_storage_without_scanning() -> Result<()> {
    let mut db = Database::memory()?;
    assert_memory_base_is_shared(&db);
    let initial = db.stats()?;
    assert_eq!(initial.generation, 1);
    assert_eq!(initial.base_entries, 0);
    assert_eq!(initial.overlay_entries, 0);
    assert_eq!(initial.overlay_log_bytes, 0);
    assert_eq!(initial.stale_bytes, 0);
    assert!(initial.base_bytes > 0);
    assert_eq!(initial.storage_bytes, DATA_START + initial.base_bytes);

    db.put(b"live", b"value")?;
    db.put(b"deleted", b"value")?;
    db.delete(b"deleted")?;
    let overlay = db.stats()?;
    assert_eq!(overlay.base_entries, 0);
    assert_eq!(overlay.overlay_entries, 2);
    assert!(overlay.overlay_log_bytes > 0);
    assert_eq!(overlay.overlay_memory_bytes, db.overlay_memory_usage());

    db.compact()?;
    assert_memory_base_is_shared(&db);
    let compacted = db.stats()?;
    assert_eq!(compacted.generation, 2);
    assert_eq!(compacted.base_entries, 1);
    assert_eq!(compacted.overlay_entries, 0);
    assert_eq!(compacted.overlay_log_bytes, 0);
    assert_eq!(compacted.stale_bytes, 0);
    assert_eq!(compacted.storage_bytes, DATA_START + compacted.base_bytes);
    Ok(())
}
