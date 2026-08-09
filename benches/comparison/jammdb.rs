use crate::common::{
    BATCH_SIZE, BUCKET, INITIAL_ENTRIES, file_size, key, mib, read_indices, temp_dir, value,
};
use ::jammdb::DB;
use std::path::{Path, PathBuf};
use test::{Bencher, black_box};

fn create(path: &Path) -> DB {
    let database = DB::open(path).unwrap();
    let transaction = database.tx(true).unwrap();
    transaction.create_bucket(BUCKET).unwrap();
    transaction.commit().unwrap();
    database
}

fn populate(database: &DB) {
    let transaction = database.tx(true).unwrap();
    let bucket = transaction.get_bucket(BUCKET).unwrap();
    for index in 0..INITIAL_ENTRIES {
        bucket
            .put(key(index).to_vec(), value(index, 0).to_vec())
            .unwrap();
    }
    drop(bucket);
    transaction.commit().unwrap();
}

fn populated() -> (tempfile::TempDir, PathBuf, DB) {
    let dir = temp_dir();
    let path = dir.path().join("db.jammdb");
    let database = create(&path);
    populate(&database);
    (dir, path, database)
}

#[bench]
fn bulk_load_100k(b: &mut Bencher) {
    let mut dirs = Vec::new();
    b.iter(|| {
        let dir = temp_dir();
        let path = dir.path().join("db.jammdb");
        let database = create(&path);
        populate(&database);
        drop(database);
        dirs.push(dir);
    });
    black_box(dirs);
}

#[bench]
fn write_single_sync(b: &mut Bencher) {
    let (_dir, _path, database) = populated();
    let mut index = INITIAL_ENTRIES;
    b.iter(|| {
        let transaction = database.tx(true).unwrap();
        let bucket = transaction.get_bucket(BUCKET).unwrap();
        bucket
            .put(key(index).to_vec(), value(index, 1).to_vec())
            .unwrap();
        drop(bucket);
        transaction.commit().unwrap();
        index += 1;
    });
}

#[bench]
fn write_batch_1000_sync(b: &mut Bencher) {
    let (_dir, _path, database) = populated();
    let mut next = INITIAL_ENTRIES;
    b.iter(|| {
        let transaction = database.tx(true).unwrap();
        let bucket = transaction.get_bucket(BUCKET).unwrap();
        for index in next..next + BATCH_SIZE {
            bucket
                .put(key(index).to_vec(), value(index, 1).to_vec())
                .unwrap();
        }
        drop(bucket);
        transaction.commit().unwrap();
        next += BATCH_SIZE;
    });
}

#[bench]
fn read_random_100k(b: &mut Bencher) {
    let (_dir, _path, database) = populated();
    let indices = read_indices();
    let transaction = database.tx(false).unwrap();
    let bucket = transaction.get_bucket(BUCKET).unwrap();
    b.iter(|| {
        let checksum = indices.iter().fold(0u64, |sum, &index| {
            let value = bucket.get_kv(key(index)).unwrap();
            sum.wrapping_add(u64::from(value.value()[0]))
        });
        black_box(checksum)
    });
}

#[bench]
fn delete_single_sync(b: &mut Bencher) {
    let (_dir, _path, database) = populated();
    let mut index = 0;
    b.iter(|| {
        assert!(index < INITIAL_ENTRIES, "delete fixture exhausted");
        let transaction = database.tx(true).unwrap();
        let bucket = transaction.get_bucket(BUCKET).unwrap();
        bucket.delete(key(index)).unwrap();
        drop(bucket);
        transaction.commit().unwrap();
        index += 1;
    });
}

#[bench]
fn size_uncompacted_compacted(b: &mut Bencher) {
    let (_dir, path, database) = populated();
    let transaction = database.tx(true).unwrap();
    let bucket = transaction.get_bucket(BUCKET).unwrap();
    for index in 0..INITIAL_ENTRIES {
        bucket
            .put(key(index).to_vec(), value(index, 1).to_vec())
            .unwrap();
    }
    for index in 0..INITIAL_ENTRIES / 2 {
        bucket.delete(key(index)).unwrap();
    }
    drop(bucket);
    transaction.commit().unwrap();
    let uncompacted = file_size(&path);
    eprintln!(
        "SIZE jammdb uncompacted={:.2}MiB compacted=N/A",
        mib(uncompacted)
    );
    b.iter(|| black_box(uncompacted));
}
