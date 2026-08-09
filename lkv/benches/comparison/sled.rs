use crate::common::{
    BATCH_SIZE, INITIAL_ENTRIES, directory_size, key, mib, read_indices, temp_dir, value,
};
use test::{Bencher, black_box};

fn create(path: &std::path::Path) -> ::sled::Db {
    ::sled::open(path).unwrap()
}

fn populate(database: &::sled::Db) {
    let mut batch = ::sled::Batch::default();
    for index in 0..INITIAL_ENTRIES {
        batch.insert(key(index).as_slice(), value(index, 0).as_slice());
    }
    database.apply_batch(batch).unwrap();
    database.flush().unwrap();
}

fn populated() -> (tempfile::TempDir, ::sled::Db) {
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
        database
            .insert(key(index), value(index, 1).as_slice())
            .unwrap();
        database.flush().unwrap();
        index += 1;
    });
}

#[bench]
fn write_batch_1000_sync(b: &mut Bencher) {
    let (_dir, database) = populated();
    let mut next = INITIAL_ENTRIES;
    b.iter(|| {
        let mut batch = ::sled::Batch::default();
        for index in next..next + BATCH_SIZE {
            batch.insert(key(index).as_slice(), value(index, 1).as_slice());
        }
        database.apply_batch(batch).unwrap();
        database.flush().unwrap();
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
        database.remove(key(index)).unwrap().unwrap();
        database.flush().unwrap();
        index += 1;
    });
}

#[bench]
fn size_uncompacted_compacted(b: &mut Bencher) {
    let (dir, database) = populated();
    let mut batch = ::sled::Batch::default();
    for index in 0..INITIAL_ENTRIES {
        batch.insert(key(index).as_slice(), value(index, 1).as_slice());
    }
    for index in 0..INITIAL_ENTRIES / 2 {
        batch.remove(key(index).as_slice());
    }
    database.apply_batch(batch).unwrap();
    database.flush().unwrap();
    let uncompacted = directory_size(dir.path());
    eprintln!(
        "SIZE sled uncompacted={:.2}MiB compacted=N/A",
        mib(uncompacted)
    );
    b.iter(|| black_box(uncompacted));
}
