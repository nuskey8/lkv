use crate::common::{
    BATCH_SIZE, INITIAL_ENTRIES, directory_size, key, mib, read_indices, temp_dir, value,
};
use ::lkv::{Database, DatabaseOptions};
use test::{Bencher, black_box};

fn create(path: &std::path::Path) -> Database {
    Database::create_with_options(
        path,
        DatabaseOptions::default().with_overlay_memory_limit(usize::MAX),
    )
    .unwrap()
}

fn populate(database: &mut Database) {
    let mut transaction = database.begin_write().unwrap();
    for index in 0..INITIAL_ENTRIES {
        transaction.put(key(index), value(index, 0)).unwrap();
    }
    transaction.commit().unwrap();
}

fn populated_base() -> (tempfile::TempDir, Database) {
    let dir = temp_dir();
    let mut database = create(&dir.path().join("database.lkv"));
    populate(&mut database);
    database.vacuum().unwrap();
    (dir, database)
}

#[bench]
fn bulk_load_100k(b: &mut Bencher) {
    let mut dirs = Vec::new();
    b.iter(|| {
        let dir = temp_dir();
        let mut database = create(&dir.path().join("database.lkv"));
        populate(&mut database);
        black_box(database.stats().unwrap());
        drop(database);
        dirs.push(dir);
    });
    black_box(dirs);
}

#[bench]
fn write_single_sync(b: &mut Bencher) {
    let (_dir, mut database) = populated_base();
    let mut index = INITIAL_ENTRIES;
    b.iter(|| {
        let mut transaction = database.begin_write().unwrap();
        transaction.put(key(index), value(index, 1)).unwrap();
        transaction.commit().unwrap();
        index += 1;
    });
}

#[bench]
fn write_batch_1000_sync(b: &mut Bencher) {
    let (_dir, mut database) = populated_base();
    let mut next = INITIAL_ENTRIES;
    b.iter(|| {
        let mut transaction = database.begin_write().unwrap();
        for index in next..next + BATCH_SIZE {
            transaction.put(key(index), value(index, 1)).unwrap();
        }
        transaction.commit().unwrap();
        next += BATCH_SIZE;
    });
}

#[bench]
fn read_random_100k(b: &mut Bencher) {
    let (_dir, database) = populated_base();
    let indices = read_indices();
    let transaction = database.begin_read().unwrap();
    b.iter(|| {
        let checksum = indices.iter().fold(0u64, |sum, &index| {
            let value = transaction.get(key(index)).unwrap().unwrap();
            sum.wrapping_add(u64::from(value[0]))
        });
        black_box(checksum)
    });
}

#[bench]
fn delete_single_sync(b: &mut Bencher) {
    let (_dir, mut database) = populated_base();
    let mut index = 0;
    b.iter(|| {
        assert!(index < INITIAL_ENTRIES, "delete fixture exhausted");
        let mut transaction = database.begin_write().unwrap();
        transaction.delete(key(index)).unwrap();
        transaction.commit().unwrap();
        index += 1;
    });
}

#[bench]
fn size_uncompacted_compacted(b: &mut Bencher) {
    let (dir, mut database) = populated_base();
    let mut transaction = database.begin_write().unwrap();
    for index in 0..INITIAL_ENTRIES {
        transaction.put(key(index), value(index, 1)).unwrap();
    }
    for index in 0..INITIAL_ENTRIES / 2 {
        transaction.delete(key(index)).unwrap();
    }
    transaction.commit().unwrap();
    let uncompacted = directory_size(dir.path());
    database.vacuum().unwrap();
    let compacted = directory_size(dir.path());
    eprintln!(
        "SIZE lkv uncompacted={:.2}MiB compacted={:.2}MiB",
        mib(uncompacted),
        mib(compacted)
    );
    b.iter(|| black_box((uncompacted, compacted)));
}
