use super::*;
use crate::format::log::{
    MAX_BATCH_OPERATIONS, Marker, record_checksum, write_batch_record, write_record_header,
};

fn append_raw_batch(path: &Path, payload: &[u8]) -> Result<()> {
    let payload_len = u32::try_from(payload.len()).unwrap();
    let checksum = record_checksum(Marker::Batch, 0, payload_len, payload);
    let mut file = OpenOptions::new().append(true).open(path)?;
    write_record_header(&mut file, Marker::Batch, 0, payload_len, checksum)?;
    file.write_all(payload)?;
    file.sync_all()?;
    Ok(())
}

#[test]
fn crash_commit_child() -> Result<()> {
    let Some(path) = std::env::var_os("LKV_TEST_CRASH_DB") else {
        return Ok(());
    };
    let operation = std::env::var("LKV_TEST_CRASH_OPERATION");
    let path = PathBuf::from(path);
    if operation.as_deref() == Ok("create") {
        let _db = Database::create(&path)?;
        panic!("configured creation crash point was not reached");
    }
    let mut db = Database::open(&path)?;
    match operation.as_deref() {
        Ok("compact") => {
            db.compact()?;
            panic!("configured compact crash point was not reached");
        }
        Ok("vacuum") => {
            db.vacuum()?;
            panic!("configured vacuum crash point was not reached");
        }
        _ => {}
    }
    let mut txn = db.begin_write()?;
    txn.put(b"a", b"new-a")?;
    txn.put(b"b", b"new-b")?;
    txn.commit()?;
    panic!("configured crash point was not reached");
}

#[test]
fn database_creation_never_publishes_a_partial_target() -> Result<()> {
    for crash_point in [
        "after_creation_file_sync",
        "after_creation_link",
        "after_creation_directory_sync",
    ] {
        let dir = temp_path();
        let status = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "database::tests::recovery::crash_commit_child",
                "--nocapture",
            ])
            .env("LKV_TEST_CRASH_DB", &dir)
            .env("LKV_TEST_CRASH_OPERATION", "create")
            .env("LKV_TEST_CRASH_POINT", crash_point)
            .status()?;
        assert_eq!(status.code(), Some(86));

        if crash_point == "after_creation_file_sync" {
            let error = Database::open(&dir)
                .err()
                .expect("an unpublished database must remain missing");
            assert_eq!(error.kind(), ErrorKind::NotFound);
            assert!(!dir.exists());
        } else {
            let db = Database::open(&dir)?;
            assert!(db.is_empty()?);
            drop(db);
        }
        remove_test_database(&dir)?;
    }
    Ok(())
}

#[test]
fn process_crash_never_exposes_a_partial_transaction() -> Result<()> {
    for crash_point in ["after_batch_write", "after_batch_sync"] {
        let dir = temp_dir();
        {
            let mut db = Database::open(&dir)?;
            let mut txn = db.begin_write()?;
            txn.put(b"a", b"old-a")?;
            txn.put(b"b", b"old-b")?;
            txn.commit()?;
        }
        let status = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "database::tests::recovery::crash_commit_child",
                "--nocapture",
            ])
            .env("LKV_TEST_CRASH_DB", &dir)
            .env("LKV_TEST_CRASH_POINT", crash_point)
            .status()?;
        assert_eq!(status.code(), Some(86));

        let db = Database::open(&dir)?;
        let a = db.get(b"a")?;
        let b = db.get(b"b")?;
        assert!(
            (a == Some(b"old-a".as_slice()) && b == Some(b"old-b".as_slice()))
                || (a == Some(b"new-a".as_slice()) && b == Some(b"new-b".as_slice())),
            "transaction was partially visible after {crash_point}: a={a:?}, b={b:?}"
        );
        drop(db);
        remove_test_database(&dir)?;
    }
    Ok(())
}

#[test]
#[cfg(not(windows))]
fn compact_and_vacuum_crash_points_remain_recoverable() -> Result<()> {
    for (operation, crash_points) in [
        (
            "compact",
            &["after_compact_base_sync", "after_compact_superblock_sync"][..],
        ),
        (
            "vacuum",
            &["after_vacuum_file_sync", "after_vacuum_rename"][..],
        ),
    ] {
        for crash_point in crash_points {
            let dir = temp_dir();
            {
                let mut db = Database::open(&dir)?;
                db.put(b"a", b"value-a")?;
                db.put(b"b", b"value-b")?;
            }
            let status = Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "database::tests::recovery::crash_commit_child",
                    "--nocapture",
                ])
                .env("LKV_TEST_CRASH_DB", &dir)
                .env("LKV_TEST_CRASH_OPERATION", operation)
                .env("LKV_TEST_CRASH_POINT", crash_point)
                .status()?;
            assert_eq!(status.code(), Some(86));

            let db = Database::open(&dir)?;
            assert_eq!(db.get(b"a")?, Some(b"value-a".as_slice()));
            assert_eq!(db.get(b"b")?, Some(b"value-b".as_slice()));
            drop(db);
            remove_test_database(&dir)?;
        }
    }
    Ok(())
}

#[test]
fn ignores_partial_log_tail() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    db.put(b"safe", b"value")?;
    db.sync()?;
    drop(db);
    OpenOptions::new()
        .append(true)
        .open(&dir)?
        .write_all(&[1, 2, 0])?;
    let db = Database::open(&dir)?;
    assert_eq!(db.get(b"safe")?, Some(b"value".as_slice()));
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn ignores_valid_header_with_partial_payload_tail() -> Result<()> {
    let dir = temp_dir();
    let db = Database::open(&dir)?;
    let log_start = db.base.log_start;
    drop(db);

    let mut payload = Vec::new();
    payload.extend_from_slice(&1u32.to_le_bytes());
    payload.push(Marker::Put as u8);
    payload.extend_from_slice(&1u32.to_le_bytes());
    payload.extend_from_slice(&1u32.to_le_bytes());
    payload.extend_from_slice(b"kv");
    let payload_len = u32::try_from(payload.len()).unwrap();
    let checksum = record_checksum(Marker::Batch, 0, payload_len, &payload);

    let path = dir.clone();
    let mut file = OpenOptions::new().append(true).open(&path)?;
    write_record_header(&mut file, Marker::Batch, 0, payload_len, checksum)?;
    file.write_all(&payload[..payload.len() / 2])?;
    file.sync_all()?;
    drop(file);

    let db = Database::open(&dir)?;
    assert!(db.is_empty()?);
    assert_eq!(fs::metadata(&path)?.len(), log_start);
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn write_transaction_is_recovered_atomically() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    let mut txn = db.begin_write()?;
    txn.put(b"a", b"one")?;
    txn.put(b"a", b"latest")?;
    txn.put(b"b", b"two")?;
    txn.delete(b"missing")?;
    assert_eq!(txn.len(), 3);
    assert_eq!(txn.get(b"a")?, Some(b"latest".as_slice()));
    txn.commit()?;
    drop(db);
    let db = Database::open(&dir)?;
    assert_eq!(db.get(b"a")?, Some(b"latest".as_slice()));
    assert_eq!(db.get(b"b")?, Some(b"two".as_slice()));
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn recovers_large_batch_without_a_payload_sized_staging_buffer() -> Result<()> {
    let dir = temp_dir();
    let value = vec![b'v'; 16 * 1024 * 1024];
    let mut db = Database::open(&dir)?;
    let mut txn = db.begin_write()?;
    txn.put(b"large", &value)?;
    txn.put(b"small", b"value")?;
    txn.commit()?;
    drop(db);

    let db = Database::open(&dir)?;
    assert_eq!(db.get(b"large")?, Some(value.as_slice()));
    assert_eq!(db.get(b"small")?, Some(b"value".as_slice()));
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn rejects_validly_checksummed_malformed_batch() -> Result<()> {
    let dir = temp_dir();
    let db = Database::open(&dir)?;
    drop(db);

    let mut payload = Vec::new();
    payload.extend_from_slice(&2u32.to_le_bytes());
    payload.push(Marker::Put as u8);
    payload.extend_from_slice(&1u32.to_le_bytes());
    payload.extend_from_slice(&3u32.to_le_bytes());
    payload.extend_from_slice(b"aone");
    payload.push(Marker::Delete as u8);
    payload.extend_from_slice(&1u32.to_le_bytes());
    payload.extend_from_slice(&1u32.to_le_bytes());
    payload.extend_from_slice(b"bx");
    append_raw_batch(&dir, &payload)?;

    let error = Database::open(&dir)
        .err()
        .expect("malformed batch must fail recovery");
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn rejects_validly_checksummed_oversized_batch_count() -> Result<()> {
    let dir = temp_dir();
    let db = Database::open(&dir)?;
    drop(db);
    let payload = ((MAX_BATCH_OPERATIONS + 1) as u32).to_le_bytes();
    append_raw_batch(&dir, &payload)?;

    let error = Database::open(&dir)
        .err()
        .expect("oversized operation count must fail recovery");
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn rejects_checksum_corruption_without_truncating_it() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    let log_start = db.base.log_start;
    db.put(b"key", b"value")?;
    db.sync()?;
    drop(db);
    let path = dir.clone();
    let original_len = fs::metadata(&path)?.len();
    let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
    file.seek(SeekFrom::Start(log_start + 9))?;
    let mut byte = [0];
    file.read_exact(&mut byte)?;
    byte[0] ^= 0xff;
    file.seek(SeekFrom::Start(log_start + 9))?;
    file.write_all(&byte)?;
    file.sync_all()?;
    let error = Database::open(&dir)
        .err()
        .expect("corruption must fail open");
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    assert_eq!(fs::metadata(&path)?.len(), original_len);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn rejects_header_length_corruption_without_truncating_committed_data() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    let log_start = db.base.log_start;
    db.put(b"key", b"value")?;
    drop(db);

    let path = dir.clone();
    let original_len = fs::metadata(&path)?.len();
    let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
    file.seek(SeekFrom::Start(log_start + 5))?;
    let mut byte = [0];
    file.read_exact(&mut byte)?;
    byte[0] ^= 0x80;
    file.seek(SeekFrom::Start(log_start + 5))?;
    file.write_all(&byte)?;
    file.sync_all()?;
    drop(file);

    let error = Database::open(&dir)
        .err()
        .expect("corrupt header length must fail open");
    assert!(matches!(error, Error::Corrupted(_)));
    assert_eq!(fs::metadata(&path)?.len(), original_len);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
#[cfg(not(windows))]
fn randomized_state_matches_hashmap_across_recovery() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    let mut model = HashMap::<Vec<u8>, Vec<u8>>::new();
    let mut random = 0x1234_5678_9abc_def0u64;
    for chunk in 0..50 {
        let mut txn = db.begin_write()?;
        for _ in 0..100 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let key = (random % 200).to_le_bytes().to_vec();
            if random >> 63 == 0 {
                let value = random.rotate_left(17).to_le_bytes().to_vec();
                txn.put(&key, &value)?;
                model.insert(key, value);
            } else {
                txn.delete(&key)?;
                model.remove(&key);
            }
        }
        txn.commit()?;
        if chunk % 5 == 0 {
            db.compact()?;
        }
        if chunk % 10 == 0 {
            db.vacuum()?;
        }
        if chunk % 3 == 0 {
            drop(db);
            db = Database::open(&dir)?;
        }
    }
    let actual: HashMap<_, _> = db
        .iter()?
        .map(|item| item.map(|(key, value)| (key.to_vec(), value.to_vec())))
        .collect::<Result<_>>()?;
    assert_eq!(actual, model);
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}

#[test]
fn partial_write_poisons_handle_and_reopen_can_continue() -> Result<()> {
    let dir = temp_dir();
    let mut db = Database::open(&dir)?;
    db.put(b"before", b"safe")?;
    let length_before = db.storage.len()?;
    let mut staged = KeyMap::default();
    staged.insert(
        b"partial".to_vec(),
        OverlayEntry::Put(ValueBytes::Owned(b"must-not-appear".to_vec())),
    );
    let value_checksums = KeyMap::default();
    let error = db
        .commit_staged_with(staged, value_checksums, |file, staged, _| {
            write_batch_record(&mut FailAfter { file, remaining: 7 }, staged)
        })
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::WriteZero);
    assert_eq!(db.storage.len()?, length_before);
    assert!(matches!(db.put(b"after", b"unsafe"), Err(Error::Poisoned)));
    drop(db);
    let mut db = Database::open(&dir)?;
    assert_eq!(db.get(b"before")?, Some(b"safe".as_slice()));
    assert_eq!(db.get(b"partial")?, None);
    db.put(b"after", b"safe-too")?;
    drop(db);
    let db = Database::open(&dir)?;
    assert_eq!(db.get(b"after")?, Some(b"safe-too".as_slice()));
    drop(db);
    remove_test_database(&dir)?;
    Ok(())
}
