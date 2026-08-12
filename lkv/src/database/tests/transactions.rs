use super::*;
use crate::database::state::{MAPPED_VALUE_THRESHOLD, OVERLAY_MAPPING_THRESHOLD};
use std::thread;

#[test]
fn write_read_delete_reopen_and_compact() -> Result<()> {
    let dir = temp_dir();
    {
        let mut db = Database::open(&dir)?;
        db.put(b"one", b"1")?;
        db.put(b"two", b"2")?;
        db.put(b"one", b"updated")?;
        db.delete(b"two")?;
        assert_eq!(db.get(b"one")?, Some(b"updated".as_slice()));
        assert_eq!(db.get(b"two")?, None);
        assert_eq!(db.len()?, 1);
        assert_eq!(
            db.iter()?
                .map(|item| item.map(|(k, v)| (k.to_vec(), v.to_vec())))
                .collect::<Result<Vec<_>>>()?,
            vec![(b"one".to_vec(), b"updated".to_vec())]
        );
        db.sync()?;
    }
    {
        let mut db = Database::open(&dir)?;
        assert_eq!(db.get(b"one")?, Some(b"updated".as_slice()));
        db.compact()?;
        assert_eq!(db.get(b"one")?, Some(b"updated".as_slice()));
        db.put(b"three", b"3")?;
    }
    let db = Database::open(&dir)?;
    assert_eq!(db.get(b"one")?, Some(b"updated".as_slice()));
    assert_eq!(db.get(b"three")?, Some(b"3".as_slice()));
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn rejects_a_second_writer() -> Result<()> {
    let dir = temp_dir();
    let db = Database::open(&dir)?;
    let error = Database::open(&dir).err().expect("second writer must fail");
    assert!(matches!(error, Error::DatabaseAlreadyOpen(_)));
    drop(db);
    let reopened = Database::open(&dir)?;
    drop(reopened);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn open_requires_an_existing_database_and_create_requires_a_new_path() -> Result<()> {
    let dir = temp_path();
    let error = Database::open(&dir)
        .err()
        .expect("open must not create a missing database");
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert!(!dir.exists());

    let db = Database::create(&dir)?;
    assert!(db.is_empty()?);
    assert!(dir.is_file());
    assert_eq!(fs::read_dir(dir.parent().unwrap())?.count(), 1);

    let error = Database::create(&dir)
        .err()
        .expect("create must not open an existing database");
    assert_eq!(error.kind(), ErrorKind::AlreadyExists);
    drop(db);

    let error = Database::create(&dir)
        .err()
        .expect("create must not replace an existing database");
    assert_eq!(error.kind(), ErrorKind::AlreadyExists);
    let mut reopened = Database::open(&dir)?;
    reopened.put(b"key", b"value")?;
    drop(reopened);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
#[cfg(unix)]
fn dropping_database_unlocks_before_a_duplicate_descriptor_closes() -> Result<()> {
    let dir = temp_path();
    let db = Database::create(&dir)?;
    let inherited = db.storage.file().unwrap().try_clone()?;

    drop(db);
    let reopened = Database::open(&dir)?;

    drop(reopened);
    drop(inherited);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn open_does_not_initialize_an_empty_file() -> Result<()> {
    let dir = temp_path();
    let path = dir.clone();
    File::create(&path)?;

    let error = Database::open(&dir)
        .err()
        .expect("an empty file must not be initialized by open");
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    assert_eq!(fs::metadata(path)?.len(), 0);

    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn write_transaction_abort_and_drop_discard_changes() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    {
        let mut txn = db.begin_write()?;
        txn.put(b"key", b"aborted")?;
        assert_eq!(txn.get(b"key")?, Some(b"aborted".as_slice()));
        txn.abort();
    }
    assert_eq!(db.begin_read()?.get(b"key")?, None);
    {
        let mut txn = db.begin_write()?;
        txn.put(b"key", b"dropped")?;
    }
    assert_eq!(db.begin_read()?.get(b"key")?, None);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn overlay_limit_requires_explicit_compaction() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open_with_options(
        &dir,
        DatabaseOptions {
            overlay_memory_limit: 32,
            ..DatabaseOptions::default()
        },
    )?;
    db.put(b"large", [b'x'; 100])?;
    let error = match db.begin_write() {
        Ok(_) => panic!("write must require maintenance"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        Error::MaintenanceRequired {
            limit: 32,
            actual: 105
        }
    ));
    assert_eq!(db.get(b"large")?, Some(&[b'x'; 100][..]));
    db.compact()?;
    assert_eq!(db.stats()?.overlay_memory_bytes, 0);
    db.begin_write()?.abort();
    assert_eq!(db.get(b"large")?, Some(&[b'x'; 100][..]));
    drop(db);
    let db = Database::open(&dir)?;
    assert_eq!(db.get(b"large")?, Some(&[b'x'; 100][..]));
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn reopen_preserves_maintenance_required_state() -> Result<()> {
    let dir = temp_dir();
    let options = DatabaseOptions {
        overlay_memory_limit: 0,
        ..DatabaseOptions::default()
    };
    let mut db = Database::open_with_options(&dir, options.clone())?;
    db.put(b"key", b"value")?;
    drop(db);

    let mut db = Database::open_with_options(&dir, options)?;
    assert_eq!(db.get(b"key")?, Some(b"value".as_slice()));
    assert!(matches!(
        db.begin_write(),
        Err(Error::MaintenanceRequired { .. })
    ));
    db.compact()?;
    db.begin_write()?.abort();
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn owned_snapshot_reads_while_writer_advances() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    db.put(b"key", b"old")?;
    let snapshot = db.snapshot()?;
    let reader = thread::spawn(move || -> Result<()> {
        for _ in 0..10_000 {
            assert_eq!(snapshot.get(b"key")?, Some(b"old".as_slice()));
        }
        Ok(())
    });
    for value in 0..100u64 {
        db.put(b"key", value.to_le_bytes())?;
    }
    reader
        .join()
        .map_err(|_| Error::other("snapshot reader panicked"))??;
    assert_eq!(db.get(b"key")?, Some(99u64.to_le_bytes().as_slice()));
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn snapshot_shares_overlay_until_the_writer_mutates_it() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    db.put(b"key", b"old")?;
    let snapshot = db.snapshot()?;
    assert!(Arc::ptr_eq(&snapshot.overlay_index, &db.overlay.index));

    db.put(b"key", b"new")?;
    assert!(!Arc::ptr_eq(&snapshot.overlay_index, &db.overlay.index));
    assert_eq!(snapshot.get(b"key")?, Some(b"old".as_slice()));
    assert_eq!(db.get(b"key")?, Some(b"new".as_slice()));
    drop(snapshot);
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn memory_database_supports_transactions_snapshots_and_maintenance() -> Result<()> {
    let mut db = Database::memory()?;
    db.put(b"key", b"old")?;
    db.put(b"gone", b"value")?;
    let snapshot = db.snapshot()?;

    let mut write = db.begin_write()?;
    write.put(b"key", b"new")?;
    write.delete(b"gone")?;
    write.put(b"other", b"two")?;
    write.commit()?;

    assert_eq!(snapshot.get(b"key")?, Some(b"old".as_slice()));
    assert_eq!(db.get(b"key")?, Some(b"new".as_slice()));
    assert_eq!(db.get(b"gone")?, None);
    db.sync()?;
    assert_eq!(snapshot.get(b"key")?, Some(b"old".as_slice()));
    let entries = snapshot
        .iter()?
        .map(|item| item.map(|(key, value)| (key.to_vec(), value.to_vec())))
        .collect::<Result<HashMap<_, _>>>()?;
    assert_eq!(entries.get(b"key".as_slice()), Some(&b"old".to_vec()));
    assert_eq!(entries.get(b"gone".as_slice()), Some(&b"value".to_vec()));
    assert_eq!(snapshot.len()?, 2);
    assert_eq!(
        db.compact()
            .expect_err("a memory database must enforce the same snapshot rule")
            .kind(),
        ErrorKind::WouldBlock
    );
    drop(snapshot);
    db.compact()?;
    assert_eq!(db.stats()?.overlay_memory_bytes, 0);
    db.verify()?;
    assert_eq!(db.get(b"other")?, Some(b"two".as_slice()));
    drop(db);
    Ok(())
}

#[test]
fn snapshot_iterator_borrows_base_and_overlay_values() -> Result<()> {
    let mut db = Database::memory()?;
    db.put(b"base", b"frozen")?;
    db.compact()?;
    db.put(b"overlay", b"latest")?;
    let snapshot = db.snapshot()?;
    let base_pointer = snapshot.get(b"base")?.unwrap().as_ptr();
    let overlay_pointer = snapshot.get(b"overlay")?.unwrap().as_ptr();

    let mut found_base = false;
    let mut found_overlay = false;
    for item in snapshot.iter()? {
        let (key, value) = item?;
        match key {
            b"base" => {
                found_base = true;
                assert_eq!(value.as_ptr(), base_pointer);
            }
            b"overlay" => {
                found_overlay = true;
                assert_eq!(value.as_ptr(), overlay_pointer);
            }
            _ => panic!("unexpected snapshot entry"),
        }
    }
    assert!(found_base && found_overlay);
    Ok(())
}

#[test]
fn large_values_are_transparently_mapped_and_survive_recovery() -> Result<()> {
    let dir = temp_dir();
    let value = vec![0x5a; MAPPED_VALUE_THRESHOLD + 137];
    let mut db = Database::open(&dir)?;
    let mut transaction = db.begin_write()?;
    transaction.put(b"large", &value)?;
    transaction.commit()?;

    assert_eq!(db.get(b"large")?, Some(value.as_slice()));
    assert_eq!(db.stats()?.overlay_memory_bytes, b"large".len());
    let snapshot = db.snapshot()?;
    drop(db);

    let mut db = Database::open(&dir)?;
    assert_eq!(db.get(b"large")?, Some(value.as_slice()));
    assert_eq!(db.stats()?.overlay_memory_bytes, b"large".len());

    let mut transaction = db.begin_write()?;
    transaction.put(b"large", b"replacement")?;
    transaction.commit()?;
    assert_eq!(db.get(b"large")?, Some(b"replacement".as_slice()));
    assert_eq!(snapshot.get(b"large")?, Some(value.as_slice()));
    drop(snapshot);
    db.compact()?;
    assert_eq!(db.get(b"large")?, Some(b"replacement".as_slice()));
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn recovered_values_share_one_overlay_mapping() -> Result<()> {
    let dir = temp_dir();
    let small = b"small mapped value";
    let large = vec![0x4d; MAPPED_VALUE_THRESHOLD + 1];
    let mut db = Database::open(&dir)?;
    db.put(b"small", small)?;
    db.put(b"large", &large)?;
    drop(db);

    let db = Database::open(&dir)?;
    assert!(matches!(
        db.overlay.index.mapped_bytes(),
        Some(BaseBytes::Mapped(_))
    ));
    assert_eq!(db.get(b"small")?, Some(small.as_slice()));
    assert_eq!(db.get(b"large")?, Some(large.as_slice()));
    assert_eq!(
        db.stats()?.overlay_memory_bytes,
        b"small".len() + small.len() + b"large".len()
    );
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn committed_file_overlay_tail_is_promoted_to_one_shared_mapping() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    db.put(b"mapped-key", b"mapped-value")?;
    assert!(db.overlay.index.mapped_bytes().is_none());

    let large = vec![0x5a; OVERLAY_MAPPING_THRESHOLD];
    db.put(b"mapping-trigger", &large)?;

    let mapped_len = {
        let mapping = db
            .overlay
            .index
            .mapped_bytes()
            .expect("file Overlay must retain one shared mapping");
        let mapping_start = mapping.as_ptr() as usize;
        let mapping_end = mapping_start + mapping.len();
        let value = db.get(b"mapped-key")?.unwrap();
        assert!((mapping_start..mapping_end).contains(&(value.as_ptr() as usize)));
        let (key, iterated_value) = db
            .iter()?
            .find(|entry| entry.as_ref().is_ok_and(|(key, _)| *key == b"mapped-key"))
            .unwrap()?;
        assert!((mapping_start..mapping_end).contains(&(key.as_ptr() as usize)));
        assert_eq!(iterated_value, value);
        mapping.len()
    };

    db.put(b"mapped-key", b"tail-value")?;
    assert_eq!(db.get(b"mapped-key")?, Some(b"tail-value".as_slice()));
    assert_eq!(db.overlay.index.mapped_bytes().unwrap().len(), mapped_len);
    assert_eq!(
        db.iter()?
            .filter(|entry| entry.as_ref().is_ok_and(|(key, _)| *key == b"mapped-key"))
            .count(),
        1
    );

    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn put_reserved_stages_exactly_sized_values() -> Result<()> {
    let dir = temp_dir();
    let value_len = MAPPED_VALUE_THRESHOLD + 31;
    let mut db = Database::open(&dir)?;
    let mut transaction = db.begin_write()?;
    transaction.put_reserved(b"large", value_len, |reserved| {
        assert_eq!(reserved.len(), value_len);
        for _ in 0..value_len {
            reserved.write_all(&[0xa5])?;
        }
        Ok(())
    })?;
    assert_eq!(transaction.get(b"large")?.unwrap().len(), value_len);
    transaction.commit()?;
    assert!(
        db.get(b"large")?
            .is_some_and(|value| value.iter().all(|byte| *byte == 0xa5))
    );
    assert_eq!(db.stats()?.overlay_memory_bytes, b"large".len());
    drop(db);

    let db = Database::open(&dir)?;
    assert!(
        db.get(b"large")?
            .is_some_and(|value| value.iter().all(|byte| *byte == 0xa5))
    );
    assert_eq!(db.stats()?.overlay_memory_bytes, b"large".len());
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn put_reserved_rejects_incomplete_and_excess_writes_without_staging() -> Result<()> {
    let mut db = Database::memory()?;
    let mut transaction = db.begin_write()?;
    let error = transaction
        .put_reserved(b"short", 4, |reserved| {
            reserved.write_all(b"abc")?;
            Ok(())
        })
        .expect_err("short reserved write must fail");
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(transaction.is_empty());

    let error = transaction
        .put_reserved(b"long", 3, |reserved| {
            reserved.write_all(b"abcd")?;
            Ok(())
        })
        .expect_err("long reserved write must fail");
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(transaction.is_empty());
    transaction.abort();
    assert!(db.is_empty()?);
    Ok(())
}

#[test]
fn cached_large_value_checksums_follow_the_final_staged_mutation() -> Result<()> {
    let dir = temp_dir();
    let large = vec![0x3c; MAPPED_VALUE_THRESHOLD + 1];
    let mut db = Database::open(&dir)?;

    let mut transaction = db.begin_write()?;
    transaction.put(b"small-final", &large)?;
    transaction.put(b"small-final", b"small")?;
    transaction.put(b"large-final", b"small")?;
    transaction.put(b"large-final", &large)?;
    transaction.put(b"deleted", &large)?;
    transaction.delete(b"deleted")?;
    transaction.commit()?;
    drop(db);

    let db = Database::open(&dir)?;
    assert_eq!(db.get(b"small-final")?, Some(b"small".as_slice()));
    assert_eq!(db.get(b"large-final")?, Some(large.as_slice()));
    assert_eq!(db.get(b"deleted")?, None);
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}
