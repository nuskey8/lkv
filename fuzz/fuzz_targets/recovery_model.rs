#![no_main]

use libfuzzer_sys::fuzz_target;
use lkv::{Database, DatabaseOptions};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

struct Input<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl Input<'_> {
    fn byte(&mut self) -> u8 {
        let byte = self.bytes[self.offset % self.bytes.len()];
        self.offset += 1;
        byte
    }

    fn number(&mut self) -> u64 {
        u64::from_le_bytes(std::array::from_fn(|_| self.byte()))
    }
}

fn apply_batch(db: &mut Database, model: &mut HashMap<Vec<u8>, Vec<u8>>, input: &mut Input<'_>) {
    let mut transaction = db.begin_write().expect("fuzz transaction must start");
    for _ in 0..1 + input.byte() as usize % 8 {
        let key = vec![input.byte() % 32, input.byte()];
        if input.byte() & 3 == 0 {
            transaction.delete(&key).expect("fuzz delete must be valid");
            model.remove(&key);
        } else {
            let value = (0..input.byte() as usize % 96)
                .map(|_| input.byte())
                .collect::<Vec<_>>();
            transaction
                .put(&key, &value)
                .expect("fuzz put must be valid");
            model.insert(key, value);
        }
    }
    transaction.commit().expect("fuzz commit must succeed");
}

fn read_model(db: &Database) -> HashMap<Vec<u8>, Vec<u8>> {
    db.iter()
        .expect("recovered database must iterate")
        .map(|entry| {
            let (key, value) = entry.expect("recovered entry must verify");
            (key.to_vec(), value.to_vec())
        })
        .collect()
}

fuzz_target!(|bytes: &[u8]| {
    if bytes.is_empty() {
        return;
    }
    let scratch =
        std::env::temp_dir().join(format!("lkv-fuzz-recovery-model-{}", std::process::id()));
    if scratch.exists() {
        fs::remove_dir_all(&scratch).expect("old fuzz scratch directory must be removable");
    }
    fs::create_dir(&scratch).expect("fuzz scratch directory must be creatable");
    let path = scratch.join("database.lkv");
    let mut input = Input { bytes, offset: 0 };
    let mut db = Database::create_with_options(
        &path,
        DatabaseOptions::default().with_overlay_memory_limit(usize::MAX),
    )
    .expect("fuzz database must be created");
    let mut model = HashMap::new();

    apply_batch(&mut db, &mut model, &mut input);
    if input.byte() & 1 != 0 {
        db.compact().expect("fuzz compaction must succeed");
    }
    let log_start = db.stats().expect("stats must succeed").storage_bytes;
    let prefix_model = model.clone();
    let mut commits = Vec::new();
    for _ in 0..1 + input.byte() as usize % 16 {
        apply_batch(&mut db, &mut model, &mut input);
        commits.push((
            db.stats().expect("stats must succeed").storage_bytes,
            model.clone(),
        ));
    }
    drop(db);

    let full_len = fs::metadata(&path)
        .expect("fuzz database metadata must exist")
        .len();
    match input.byte() % 3 {
        0 => {
            let cut = log_start + input.number() % (full_len - log_start + 1);
            let file = OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("fuzz database must open for truncation");
            file.set_len(cut).expect("fuzz truncation must succeed");
            file.sync_all().expect("fuzz truncation must sync");
            drop(file);

            let expected = commits
                .iter()
                .rev()
                .find(|(end, _)| *end <= cut)
                .map_or(&prefix_model, |(_, model)| model);
            let db = Database::open(&path).expect("durable prefix must recover");
            assert_eq!(&read_model(&db), expected);
            let expected_len = commits
                .iter()
                .rev()
                .find(|(end, _)| *end <= cut)
                .map_or(log_start, |(end, _)| *end);
            assert_eq!(
                db.stats().expect("stats must succeed").storage_bytes,
                expected_len
            );
        }
        1 => {
            let offset = log_start + input.number() % (full_len - log_start);
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .expect("fuzz database must open for corruption");
            file.seek(SeekFrom::Start(offset))
                .expect("corruption seek must succeed");
            let mut byte = [0];
            file.read_exact(&mut byte)
                .expect("corruption read must succeed");
            byte[0] ^= 1 << (input.byte() & 7);
            file.seek(SeekFrom::Start(offset))
                .expect("corruption seek must succeed");
            file.write_all(&byte)
                .expect("corruption write must succeed");
            file.sync_all().expect("corruption must sync");
            drop(file);
            assert!(
                Database::open(&path).is_err(),
                "committed single-bit corruption must be rejected"
            );
        }
        _ => {
            let db = Database::open(&path).expect("clean fuzz database must reopen");
            assert_eq!(read_model(&db), model);
        }
    }
    fs::remove_dir_all(&scratch).expect("fuzz scratch directory must be removable");
});
