use crate::common::{
    BATCH_SIZE, INITIAL_ENTRIES, directory_size, key, mib, read_indices, temp_dir, value,
};
use ::fjall::{Database, Keyspace, KeyspaceCreateOptions, PersistMode};
use test::{Bencher, black_box};

fn create(path: &std::path::Path) -> (Database, Keyspace) {
    let database = Database::builder(path).open().unwrap();
    let keyspace = database
        .keyspace("kv", KeyspaceCreateOptions::default)
        .unwrap();
    (database, keyspace)
}

fn populate(database: &Database, keyspace: &Keyspace) {
    let mut batch = database.batch().durability(Some(PersistMode::SyncAll));
    for index in 0..INITIAL_ENTRIES {
        batch.insert(keyspace, key(index).as_slice(), value(index, 0).as_slice());
    }
    batch.commit().unwrap();
}

fn populated() -> (tempfile::TempDir, Database, Keyspace) {
    let dir = temp_dir();
    let (database, keyspace) = create(dir.path());
    populate(&database, &keyspace);
    (dir, database, keyspace)
}

#[bench]
fn bulk_load_100k(b: &mut Bencher) {
    let mut dirs = Vec::new();
    b.iter(|| {
        let dir = temp_dir();
        let (database, keyspace) = create(dir.path());
        populate(&database, &keyspace);
        drop(keyspace);
        drop(database);
        dirs.push(dir);
    });
    black_box(dirs);
}

#[bench]
fn write_single_sync(b: &mut Bencher) {
    let (_dir, database, keyspace) = populated();
    let mut index = INITIAL_ENTRIES;
    b.iter(|| {
        let mut batch = database.batch().durability(Some(PersistMode::SyncAll));
        batch.insert(&keyspace, key(index).as_slice(), value(index, 1).as_slice());
        batch.commit().unwrap();
        index += 1;
    });
}

#[bench]
fn write_batch_1000_sync(b: &mut Bencher) {
    let (_dir, database, keyspace) = populated();
    let mut next = INITIAL_ENTRIES;
    b.iter(|| {
        let mut batch = database.batch().durability(Some(PersistMode::SyncAll));
        for index in next..next + BATCH_SIZE {
            batch.insert(&keyspace, key(index).as_slice(), value(index, 1).as_slice());
        }
        batch.commit().unwrap();
        next += BATCH_SIZE;
    });
}

#[bench]
fn read_random_100k(b: &mut Bencher) {
    let (_dir, _database, keyspace) = populated();
    let indices = read_indices();
    b.iter(|| {
        let checksum = indices.iter().fold(0u64, |sum, &index| {
            let value = keyspace.get(key(index)).unwrap().unwrap();
            sum.wrapping_add(u64::from(value[0]))
        });
        black_box(checksum)
    });
}

#[bench]
fn delete_single_sync(b: &mut Bencher) {
    let (_dir, database, keyspace) = populated();
    let mut index = 0;
    b.iter(|| {
        assert!(index < INITIAL_ENTRIES, "delete fixture exhausted");
        let mut batch = database.batch().durability(Some(PersistMode::SyncAll));
        batch.remove(&keyspace, key(index));
        batch.commit().unwrap();
        index += 1;
    });
}

#[bench]
fn size_uncompacted_compacted(b: &mut Bencher) {
    let (dir, database, keyspace) = populated();
    let mut batch = database.batch().durability(Some(PersistMode::SyncAll));
    for index in 0..INITIAL_ENTRIES {
        batch.insert(&keyspace, key(index), value(index, 1));
    }
    for index in 0..INITIAL_ENTRIES / 2 {
        batch.remove(&keyspace, key(index));
    }
    batch.commit().unwrap();
    keyspace.rotate_memtable_and_wait().unwrap();
    let uncompacted = directory_size(dir.path());
    eprintln!(
        "SIZE fjall uncompacted={:.2}MiB compacted=N/A",
        mib(uncompacted)
    );
    b.iter(|| black_box(uncompacted));
}
