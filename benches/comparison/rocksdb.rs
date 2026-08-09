use crate::common::{
    BATCH_SIZE, INITIAL_ENTRIES, directory_size, key, mib, read_indices, temp_dir, value,
};
use ::rocksdb::{DB, Options, WriteBatch, WriteOptions};
use std::path::Path;
use test::{Bencher, black_box};

fn create(path: &Path) -> DB {
    let mut options = Options::default();
    options.create_if_missing(true);
    DB::open(&options, path).unwrap()
}

fn write_sync(database: &DB, batch: WriteBatch) {
    let mut options = WriteOptions::default();
    options.set_sync(true);
    database.write_opt(batch, &options).unwrap();
    sync_database_files(database.path());
}

// RocksDB's sync write did not provide the durability expected by redb's
// benchmark on macOS. Keep its explicit file sync so commit semantics remain
// comparable on that platform.
#[cfg(target_os = "macos")]
fn sync_database_files(path: &Path) {
    for entry in std::fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            match std::fs::File::open(entry.path()) {
                Ok(file) => file.sync_all().unwrap(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // Background compaction removed the file after read_dir().
                }
                Err(error) => panic!("open RocksDB file for sync: {error}"),
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn sync_database_files(_path: &Path) {}

fn populate(database: &DB) {
    let mut batch = WriteBatch::default();
    for index in 0..INITIAL_ENTRIES {
        batch.put(key(index), value(index, 0));
    }
    write_sync(database, batch);
}

fn populated() -> (tempfile::TempDir, DB) {
    let dir = temp_dir();
    let database = create(dir.path());
    populate(&database);
    (dir, database)
}

#[bench]
fn bulk_load_100k(b: &mut Bencher) {
    let mut dirs = Vec::new();
    b.iter(|| {
        let dir = temp_dir();
        let database = create(dir.path());
        populate(&database);
        drop(database);
        dirs.push(dir);
    });
    black_box(dirs);
}

#[bench]
fn write_single_sync(b: &mut Bencher) {
    let (_dir, database) = populated();
    let mut index = INITIAL_ENTRIES;
    b.iter(|| {
        let mut batch = WriteBatch::default();
        batch.put(key(index), value(index, 1));
        write_sync(&database, batch);
        index += 1;
    });
}

#[bench]
fn write_batch_1000_sync(b: &mut Bencher) {
    let (_dir, database) = populated();
    let mut next = INITIAL_ENTRIES;
    b.iter(|| {
        let mut batch = WriteBatch::default();
        for index in next..next + BATCH_SIZE {
            batch.put(key(index), value(index, 1));
        }
        write_sync(&database, batch);
        next += BATCH_SIZE;
    });
}

#[bench]
fn read_random_100k(b: &mut Bencher) {
    let (_dir, database) = populated();
    let indices = read_indices();
    b.iter(|| {
        let checksum = indices.iter().fold(0u64, |sum, &index| {
            let value = database.get(key(index)).unwrap().unwrap();
            sum.wrapping_add(u64::from(value[0]))
        });
        black_box(checksum)
    });
}

#[bench]
fn delete_single_sync(b: &mut Bencher) {
    let (_dir, database) = populated();
    let mut index = 0;
    b.iter(|| {
        assert!(index < INITIAL_ENTRIES, "delete fixture exhausted");
        let mut batch = WriteBatch::default();
        batch.delete(key(index));
        write_sync(&database, batch);
        index += 1;
    });
}

#[bench]
fn size_uncompacted_compacted(b: &mut Bencher) {
    let (dir, database) = populated();
    let mut batch = WriteBatch::default();
    for index in 0..INITIAL_ENTRIES {
        batch.put(key(index), value(index, 1));
    }
    for index in 0..INITIAL_ENTRIES / 2 {
        batch.delete(key(index));
    }
    write_sync(&database, batch);
    database.flush().unwrap();
    let uncompacted = directory_size(dir.path());
    database.compact_range::<&[u8], &[u8]>(None, None);
    let compacted = directory_size(dir.path());
    eprintln!(
        "SIZE rocksdb uncompacted={:.2}MiB compacted={:.2}MiB",
        mib(uncompacted),
        mib(compacted)
    );
    b.iter(|| black_box((uncompacted, compacted)));
}
