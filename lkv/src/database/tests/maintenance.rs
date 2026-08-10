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
fn single_file_compact_and_vacuum() -> Result<()> {
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
    {
        let mut txn = db.begin_write()?;
        for i in 0..1_000u32 {
            txn.put(i.to_le_bytes(), [b'y'; 100])?;
        }
        txn.commit()?;
    }
    db.compact()?;
    let before = fs::metadata(&dir)?.len();
    db.vacuum()?;
    let after = fs::metadata(&dir)?.len();
    assert!(after < before);
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
fn vacuum_requires_snapshots_to_be_dropped() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    db.put(b"key", b"value")?;
    let snapshot = db.snapshot()?;

    let error = db
        .vacuum()
        .expect_err("a live snapshot must prevent vacuum");
    assert_eq!(error.kind(), ErrorKind::WouldBlock);
    assert_eq!(db.get(b"key")?, Some(b"value".as_slice()));

    drop(snapshot);
    db.vacuum()?;
    assert_eq!(db.get(b"key")?, Some(b"value".as_slice()));
    assert!(matches!(&db.base.mapping, BaseBytes::Mapped(_)));
    assert!(!db.storage.is_memory());
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
    db.vacuum()?;

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
    assert!(compacted.stale_bytes > 0);

    db.vacuum()?;
    assert_memory_base_is_shared(&db);
    let vacuumed = db.stats()?;
    assert_eq!(vacuumed.generation, 3);
    assert_eq!(vacuumed.base_entries, 1);
    assert_eq!(vacuumed.overlay_entries, 0);
    assert_eq!(vacuumed.stale_bytes, 0);
    assert_eq!(vacuumed.storage_bytes, DATA_START + vacuumed.base_bytes);
    Ok(())
}

#[test]
fn stale_vacuum_file_is_inspected_and_removed_explicitly() -> Result<()> {
    let dir = temp_dir();
    let db = Database::open(&dir)?;
    let temporary = super::super::maintenance::compacting_path(
        db.path.as_deref().expect("persistent test database"),
    );
    fs::write(&temporary, b"partial vacuum")?;
    assert!(db.has_stale_vacuum()?);
    assert!(db.remove_stale_vacuum()?);
    assert!(!db.has_stale_vacuum()?);
    assert!(!db.remove_stale_vacuum()?);

    let backup = super::super::maintenance::vacuum_backup_path(
        db.path.as_deref().expect("persistent test database"),
    );
    fs::write(&backup, b"old vacuum generation")?;
    assert!(db.has_stale_vacuum()?);
    assert!(db.remove_stale_vacuum()?);
    assert!(!db.has_stale_vacuum()?);
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn vacuum_refuses_a_compacting_symlink_without_touching_its_target() -> Result<()> {
    use std::os::unix::fs::symlink;

    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    db.put(b"key", b"value")?;
    let victim = dir.with_file_name("victim");
    fs::write(&victim, b"must survive")?;
    symlink(
        &victim,
        super::super::maintenance::compacting_path(
            db.path.as_deref().expect("persistent test database"),
        ),
    )?;

    let error = db.vacuum().expect_err("existing symlink must be refused");
    assert_eq!(error.kind(), ErrorKind::AlreadyExists);
    assert_eq!(
        db.remove_stale_vacuum().unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(fs::read(&victim)?, b"must survive");
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}
