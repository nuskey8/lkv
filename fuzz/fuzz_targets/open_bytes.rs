#![no_main]

use libfuzzer_sys::fuzz_target;
use lkv::{Database, DatabaseOptions, VerificationMode};
use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::sync::OnceLock;

static VALID_SEED: OnceLock<Vec<u8>> = OnceLock::new();

fn exercise(path: &Path, bytes: &[u8], verification: VerificationMode) {
    let database_path = path.join("database.lkv");
    fs::write(&database_path, bytes).expect("fuzz scratch file must be writable");
    let options = DatabaseOptions::default().with_verification(verification);
    let Ok(db) = Database::open_with_options(&database_path, options) else {
        return;
    };

    let key_len = bytes.len().min(64);
    let key = &bytes[..key_len];
    let _ = black_box(db.get(key));
    let _ = black_box(db.contains_key(key));
    let _ = black_box(db.len());
    if let Ok(mut entries) = db.iter() {
        for entry in entries.by_ref().take(1_024) {
            let _ = black_box(entry);
        }
    }
    if let Ok(snapshot) = db.snapshot() {
        let _ = black_box(snapshot.get(key));
        if let Ok(entries) = snapshot.iter() {
            for entry in entries {
                let _ = black_box(entry);
            }
        }
    }
    let _ = black_box(db.verify());
}

fn valid_seed(path: &Path) -> Vec<u8> {
    let database_path = path.join("database.lkv");
    let mut db = Database::create(&database_path).expect("seed database must be created");
    let mut transaction = db.begin_write().expect("write transaction must start");
    transaction
        .put(b"alpha", b"one")
        .expect("seed put must work");
    transaction
        .put(b"beta", b"two")
        .expect("seed put must work");
    transaction.commit().expect("seed commit must work");
    db.compact().expect("seed compact must work");
    drop(db);
    fs::read(database_path).expect("fuzz seed file must be readable")
}

fuzz_target!(|bytes: &[u8]| {
    let scratch = std::env::temp_dir().join(format!("lkv-fuzz-open-{}", std::process::id()));
    if scratch.exists() {
        fs::remove_dir_all(&scratch).expect("old fuzz scratch directory must be removable");
    }
    fs::create_dir(&scratch).expect("fuzz scratch directory must be creatable");

    let seed = VALID_SEED.get_or_init(|| valid_seed(&scratch)).clone();
    let mut mutated = seed.clone();
    for (index, byte) in bytes.iter().copied().enumerate() {
        let target = index
            .wrapping_mul(0x9e37_79b9usize)
            .wrapping_add(byte as usize)
            % mutated.len();
        mutated[target] ^= byte.rotate_left((index & 7) as u32);
    }
    let mut appended = seed;
    appended.extend_from_slice(bytes);

    for verification in [VerificationMode::OnRead, VerificationMode::Full] {
        exercise(&scratch, bytes, verification);
        exercise(&scratch, &mutated, verification);
        exercise(&scratch, &appended, verification);
    }
    fs::remove_dir_all(&scratch).expect("fuzz scratch directory must be removable");
});
