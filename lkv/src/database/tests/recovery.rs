use super::*;
use crate::database::state::OVERLAY_MAPPING_THRESHOLD;
use crate::format::log::{
    LOG_HEADER_SIZE, MAX_BATCH_OPERATIONS, Marker, record_checksum, write_batch_record,
    write_record_header,
};
use std::collections::BTreeSet;

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
    if operation.as_deref() == Ok("compact") {
        db.compact()?;
        panic!("configured compact crash point was not reached");
    }
    let mut txn = db.begin_write()?;
    match operation.as_deref() {
        Ok("mixed") | Ok("promotion") => {
            txn.put(b"a", b"new-a")?;
            txn.delete(b"b")?;
            txn.put(b"c", b"new-c")?;
            txn.delete(b"d")?;
            if operation.as_deref() == Ok("promotion") {
                txn.put(b"trigger", vec![0x6d; OVERLAY_MAPPING_THRESHOLD])?;
            }
        }
        _ => {
            txn.put(b"a", b"new-a")?;
            txn.put(b"b", b"new-b")?;
        }
    }
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
    for crash_point in [
        "after_batch_write",
        "after_batch_sync",
        "after_batch_publish",
    ] {
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

#[derive(Clone, Copy)]
enum OverlayCrashFixture {
    Empty,
    Tail,
    Mapped,
    PromoteTail,
    Remap,
}

impl OverlayCrashFixture {
    fn name(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Tail => "tail",
            Self::Mapped => "mapped",
            Self::PromoteTail => "promote-tail",
            Self::Remap => "remap",
        }
    }

    fn has_initial_keys(self) -> bool {
        !matches!(self, Self::Empty)
    }

    fn has_mapped_prefix(self) -> bool {
        matches!(self, Self::Mapped | Self::Remap)
    }

    fn promotes(self) -> bool {
        matches!(self, Self::PromoteTail | Self::Remap)
    }
}

fn prepare_overlay_crash_fixture(path: &Path, fixture: OverlayCrashFixture) -> Result<()> {
    let mut db = Database::open(path)?;
    if fixture.has_initial_keys() {
        let mut txn = db.begin_write()?;
        txn.put(b"a", b"old-a")?;
        txn.put(b"b", b"old-b")?;
        txn.commit()?;
    }
    if fixture.has_mapped_prefix() {
        db.put(b"seed", vec![0x53; OVERLAY_MAPPING_THRESHOLD])?;
    }
    drop(db);
    Ok(())
}

fn crash_commit(path: &Path, fixture: OverlayCrashFixture, crash_point: &str) -> Result<()> {
    let operation = if fixture.promotes() {
        "promotion"
    } else {
        "mixed"
    };
    let status = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "database::tests::recovery::crash_commit_child",
            "--nocapture",
        ])
        .env("LKV_TEST_CRASH_DB", path)
        .env("LKV_TEST_CRASH_OPERATION", operation)
        .env("LKV_TEST_CRASH_POINT", crash_point)
        .status()?;
    assert_eq!(
        status.code(),
        Some(86),
        "{} fixture did not crash at {crash_point}",
        fixture.name()
    );
    Ok(())
}

fn overlay_crash_state(path: &Path, fixture: OverlayCrashFixture) -> Result<&'static str> {
    let db = Database::open(path)?;
    let a = db.get(b"a")?;
    let b = db.get(b"b")?;
    let c = db.get(b"c")?;
    let d = db.get(b"d")?;
    let trigger = db.get(b"trigger")?;
    let seed = db.get(b"seed")?;
    let entries = db.stats()?.overlay_entries;

    let old_entries =
        usize::from(fixture.has_initial_keys()) * 2 + usize::from(fixture.has_mapped_prefix());
    let new_entries =
        4 + usize::from(fixture.has_mapped_prefix()) + usize::from(fixture.promotes());
    let is_old = a == fixture.has_initial_keys().then_some(b"old-a".as_slice())
        && b == fixture.has_initial_keys().then_some(b"old-b".as_slice())
        && c.is_none()
        && d.is_none()
        && trigger.is_none()
        && entries == old_entries;
    let is_new = a == Some(b"new-a".as_slice())
        && b.is_none()
        && c == Some(b"new-c".as_slice())
        && d.is_none()
        && trigger
            .is_some_and(|value| fixture.promotes() && value.len() == OVERLAY_MAPPING_THRESHOLD)
            == fixture.promotes()
        && entries == new_entries;
    assert_eq!(
        seed.is_some_and(|value| value.len() == OVERLAY_MAPPING_THRESHOLD),
        fixture.has_mapped_prefix(),
        "{} fixture lost its mapped prefix",
        fixture.name()
    );
    match (is_old, is_new) {
        (true, false) => Ok("old"),
        (false, true) => Ok("new"),
        _ => panic!(
            "{} fixture recovered a partial transaction: a={a:?} b={b:?} c={c:?} d={d:?} trigger_len={:?} entries={entries}",
            fixture.name(),
            trigger.map(<[u8]>::len)
        ),
    }
}

#[test]
fn crash_matrix_preserves_atomicity_across_overlay_storage_transitions() -> Result<()> {
    for fixture in [
        OverlayCrashFixture::Empty,
        OverlayCrashFixture::Tail,
        OverlayCrashFixture::Mapped,
        OverlayCrashFixture::PromoteTail,
        OverlayCrashFixture::Remap,
    ] {
        for crash_point in [
            "after_batch_write",
            "after_batch_sync",
            "after_batch_publish",
        ] {
            let path = temp_dir();
            prepare_overlay_crash_fixture(&path, fixture)?;
            crash_commit(&path, fixture, crash_point)?;
            let state = overlay_crash_state(&path, fixture)?;
            if crash_point != "after_batch_write" {
                assert_eq!(
                    state,
                    "new",
                    "{} fixture lost a synced transaction after {crash_point}",
                    fixture.name()
                );
            }
            remove_test_database(&path)?;
        }
    }
    Ok(())
}

#[test]
fn compact_crash_points_remain_recoverable() -> Result<()> {
    for crash_point in [
        "after_compact_marker_write",
        "after_compact_base_write",
        "after_compact_base_sync",
        "after_compact_superblock_write",
        "after_compact_superblock_sync",
        "after_compact_destination_base_write",
        "after_compact_relocation_marker_write",
        "after_compact_relocation_sync",
        "after_compact_destination_superblock_write",
        "after_compact_destination_superblock_sync",
        "after_compact_redundant_superblock_write",
        "after_compact_redundant_superblock_sync",
        "after_compact_truncate",
        "after_compact_truncate_sync",
    ] {
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
            .env("LKV_TEST_CRASH_OPERATION", "compact")
            .env("LKV_TEST_CRASH_POINT", crash_point)
            .status()?;
        assert_eq!(status.code(), Some(86));

        let db = Database::open(&dir)?;
        assert_eq!(db.get(b"a")?, Some(b"value-a".as_slice()));
        assert_eq!(db.get(b"b")?, Some(b"value-b".as_slice()));
        drop(db);
        remove_test_database(&dir)?;
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
    drop(db);
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
fn every_truncated_batch_byte_boundary_recovers_the_previous_commit() -> Result<()> {
    let mut staged = KeyMap::default();
    staged.insert(
        b"safe".to_vec(),
        OverlayEntry::Put(ValueBytes::Owned(b"replacement".to_vec())),
    );
    staged.insert(
        b"inserted".to_vec(),
        OverlayEntry::Put(ValueBytes::Owned(b"new-value".to_vec())),
    );
    staged.insert(b"missing".to_vec(), OverlayEntry::Delete);
    let mut record = Vec::new();
    write_batch_record(&mut record, &staged)?;

    for cut in 0..record.len() {
        let path = temp_dir();
        let mut db = Database::open(&path)?;
        db.put(b"safe", b"previous")?;
        let committed_len = db.storage.len()?;
        drop(db);

        let mut file = OpenOptions::new().append(true).open(&path)?;
        file.write_all(&record[..cut])?;
        file.sync_all()?;
        drop(file);

        let db = Database::open(&path)?;
        assert_eq!(
            db.get(b"safe")?,
            Some(b"previous".as_slice()),
            "partial batch became visible at byte {cut}/{}",
            record.len()
        );
        assert_eq!(db.get(b"inserted")?, None, "insert visible at byte {cut}");
        assert_eq!(db.get(b"missing")?, None, "delete visible at byte {cut}");
        assert_eq!(
            db.storage.len()?,
            committed_len,
            "partial tail was not truncated at byte {cut}"
        );
        drop(db);
        remove_test_database(&path)?;
    }
    Ok(())
}

#[derive(Clone)]
struct FuzzBatch {
    start: u64,
    end: u64,
    model: HashMap<Vec<u8>, Vec<u8>>,
}

fn fuzz_random(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state
}

fn build_recovery_fuzz_fixture() -> Result<(PathBuf, u64, Vec<FuzzBatch>)> {
    let path = temp_dir();
    let mut db = Database::open(&path)?;
    let log_start = db.base.log_start;
    let mut model = HashMap::<Vec<u8>, Vec<u8>>::new();
    let mut batches = Vec::new();
    let mut random = 0xd1b5_4a32_d192_ed03;

    for batch_index in 0..24u64 {
        let start = db.storage.len()?;
        let mut txn = db.begin_write()?;
        for operation_index in 0..9u64 {
            let value = fuzz_random(&mut random);
            let key = (value % 41).to_le_bytes().to_vec();
            if value & 3 == 0 {
                txn.delete(&key)?;
                model.remove(&key);
            } else {
                let value_len = 1 + ((value >> 8) as usize % 193);
                let mut bytes = vec![0; value_len];
                for (index, byte) in bytes.iter_mut().enumerate() {
                    *byte = value
                        .wrapping_add(batch_index)
                        .wrapping_add(operation_index)
                        .wrapping_add(index as u64) as u8;
                }
                txn.put(&key, &bytes)?;
                model.insert(key, bytes);
            }
        }
        txn.commit()?;
        let end = db.storage.len()?;
        batches.push(FuzzBatch {
            start,
            end,
            model: model.clone(),
        });
    }
    drop(db);
    Ok((path, log_start, batches))
}

fn assert_database_model(path: &Path, expected: &HashMap<Vec<u8>, Vec<u8>>) -> Result<()> {
    let db = Database::open(path)?;
    let actual = db
        .iter()?
        .map(|entry| entry.map(|(key, value)| (key.to_vec(), value.to_vec())))
        .collect::<Result<HashMap<_, _>>>()?;
    assert_eq!(&actual, expected);
    drop(db);
    Ok(())
}

#[test]
fn recovery_fuzz_matches_every_surviving_durable_prefix() -> Result<()> {
    let (source, log_start, batches) = build_recovery_fuzz_fixture()?;
    let full_len = batches.last().unwrap().end;
    let mut cuts = BTreeSet::from([log_start, full_len]);
    for batch in &batches {
        for cut in [
            batch.start,
            batch.start + 1,
            (batch.start + LOG_HEADER_SIZE as u64 - 1).min(batch.end - 1),
            (batch.start + batch.end) / 2,
            batch.end - 1,
            batch.end,
        ] {
            cuts.insert(cut);
        }
    }
    let mut random = 0x94d0_49bb_1331_11ebu64;
    for _ in 0..96 {
        cuts.insert(log_start + fuzz_random(&mut random) % (full_len - log_start + 1));
    }

    let empty = HashMap::new();
    for cut in cuts {
        let target = copy_test_database(&source)?;
        let file = OpenOptions::new().write(true).open(&target)?;
        file.set_len(cut)?;
        file.sync_all()?;
        drop(file);

        let surviving = batches.iter().rev().find(|batch| batch.end <= cut);
        let expected = surviving.map_or(&empty, |batch| &batch.model);
        let expected_len = surviving.map_or(log_start, |batch| batch.end);
        assert_database_model(&target, expected)?;
        assert_eq!(
            fs::metadata(&target)?.len(),
            expected_len,
            "recovery retained an incomplete batch after truncation at {cut}"
        );
        remove_test_database(&target)?;
    }
    remove_test_database(&source)?;
    Ok(())
}

#[test]
fn recovery_fuzz_rejects_single_bit_corruption_in_committed_log() -> Result<()> {
    let (source, log_start, batches) = build_recovery_fuzz_fixture()?;
    let full_len = batches.last().unwrap().end;
    let mut offsets = BTreeSet::new();
    for batch in &batches {
        for offset in [
            batch.start,
            batch.start + 5,
            batch.start + 13,
            batch.start + LOG_HEADER_SIZE as u64,
            (batch.start + batch.end) / 2,
            batch.end - 1,
        ] {
            offsets.insert(offset);
        }
    }
    let mut random = 0xa076_1d64_78bd_642fu64;
    for _ in 0..96 {
        offsets.insert(log_start + fuzz_random(&mut random) % (full_len - log_start));
    }

    for offset in offsets {
        let target = copy_test_database(&source)?;
        let original_len = fs::metadata(&target)?.len();
        let mut file = OpenOptions::new().read(true).write(true).open(&target)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut byte = [0];
        file.read_exact(&mut byte)?;
        byte[0] ^= 1;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(&byte)?;
        file.sync_all()?;
        drop(file);

        let error = Database::open(&target)
            .err()
            .unwrap_or_else(|| panic!("corruption at file offset {offset} was accepted"));
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(
            fs::metadata(&target)?.len(),
            original_len,
            "corruption at {offset} was mistaken for a partial tail"
        );
        remove_test_database(&target)?;
    }
    remove_test_database(&source)?;
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
    #[cfg(not(windows))]
    assert_eq!(db.storage.len()?, length_before);
    #[cfg(windows)]
    assert!(db.storage.len()? > length_before);
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

fn partial_write_staged() -> KeyMap<OverlayEntry> {
    KeyMap::from_iter([
        (
            b"before".to_vec(),
            OverlayEntry::Put(ValueBytes::Owned(b"replacement".to_vec())),
        ),
        (
            b"inserted".to_vec(),
            OverlayEntry::Put(ValueBytes::Owned(b"new-value".to_vec())),
        ),
        (b"deleted".to_vec(), OverlayEntry::Delete),
    ])
}

#[test]
fn injected_write_failure_at_every_batch_byte_preserves_previous_commit() -> Result<()> {
    let mut serialized = Vec::new();
    write_batch_record(&mut serialized, &partial_write_staged())?;
    let path = temp_dir();
    let mut db = Database::open(&path)?;
    db.put(b"before", b"safe")?;
    db.put(b"deleted", b"still-present")?;
    drop(db);

    for remaining in 0..serialized.len() {
        let mut db = Database::open(&path)?;
        let length_before = db.storage.len()?;
        let error = db
            .commit_staged_with(
                partial_write_staged(),
                KeyMap::default(),
                |file, staged, _| write_batch_record(&mut FailAfter { file, remaining }, staged),
            )
            .expect_err("injected partial write must fail");
        assert_eq!(error.kind(), ErrorKind::WriteZero);
        assert!(matches!(db.put(b"unsafe", b"value"), Err(Error::Poisoned)));
        drop(db);

        let db = Database::open(&path)?;
        assert_eq!(db.get(b"before")?, Some(b"safe".as_slice()));
        assert_eq!(db.get(b"deleted")?, Some(b"still-present".as_slice()));
        assert_eq!(db.get(b"inserted")?, None);
        assert_eq!(
            db.storage.len()?,
            length_before,
            "failed write was not removed at byte {remaining}/{}",
            serialized.len()
        );
        drop(db);
    }
    remove_test_database(&path)?;
    Ok(())
}
