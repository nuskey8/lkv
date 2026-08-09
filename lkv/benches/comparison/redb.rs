use crate::common::{
    BATCH_SIZE, INITIAL_ENTRIES, file_size, key, mib, read_indices, temp_dir, value,
};
use ::redb::{Database, ReadableDatabase, TableDefinition};
use test::{Bencher, black_box};

const TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("kv");

fn create(path: &std::path::Path) -> Database {
    Database::create(path).unwrap()
}

fn populate(database: &Database) {
    let transaction = database.begin_write().unwrap();
    {
        let mut table = transaction.open_table(TABLE).unwrap();
        for index in 0..INITIAL_ENTRIES {
            table
                .insert(key(index).as_slice(), value(index, 0).as_slice())
                .unwrap();
        }
    }
    transaction.commit().unwrap();
}

fn populated() -> (tempfile::TempDir, std::path::PathBuf, Database) {
    let dir = temp_dir();
    let path = dir.path().join("db.redb");
    let database = create(&path);
    populate(&database);
    (dir, path, database)
}

#[bench]
fn bulk_load_100k(b: &mut Bencher) {
    let mut dirs = Vec::new();
    b.iter(|| {
        let dir = temp_dir();
        let path = dir.path().join("db.redb");
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
        let transaction = database.begin_write().unwrap();
        {
            let mut table = transaction.open_table(TABLE).unwrap();
            table
                .insert(key(index).as_slice(), value(index, 1).as_slice())
                .unwrap();
        }
        transaction.commit().unwrap();
        index += 1;
    });
}

#[bench]
fn write_batch_1000_sync(b: &mut Bencher) {
    let (_dir, _path, database) = populated();
    let mut next = INITIAL_ENTRIES;
    b.iter(|| {
        let transaction = database.begin_write().unwrap();
        {
            let mut table = transaction.open_table(TABLE).unwrap();
            for index in next..next + BATCH_SIZE {
                table
                    .insert(key(index).as_slice(), value(index, 1).as_slice())
                    .unwrap();
            }
        }
        transaction.commit().unwrap();
        next += BATCH_SIZE;
    });
}

#[bench]
fn read_random_100k(b: &mut Bencher) {
    let (_dir, _path, database) = populated();
    let indices = read_indices();
    let transaction = database.begin_read().unwrap();
    let table = transaction.open_table(TABLE).unwrap();
    b.iter(|| {
        let checksum = indices.iter().fold(0u64, |sum, &index| {
            let value = table.get(key(index).as_slice()).unwrap().unwrap();
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
        let transaction = database.begin_write().unwrap();
        {
            let mut table = transaction.open_table(TABLE).unwrap();
            table.remove(key(index).as_slice()).unwrap().unwrap();
        }
        transaction.commit().unwrap();
        index += 1;
    });
}

#[bench]
fn size_uncompacted_compacted(b: &mut Bencher) {
    let (_dir, path, mut database) = populated();
    let transaction = database.begin_write().unwrap();
    {
        let mut table = transaction.open_table(TABLE).unwrap();
        for index in 0..INITIAL_ENTRIES {
            table
                .insert(key(index).as_slice(), value(index, 1).as_slice())
                .unwrap();
        }
        for index in 0..INITIAL_ENTRIES / 2 {
            table.remove(key(index).as_slice()).unwrap().unwrap();
        }
    }
    transaction.commit().unwrap();
    let uncompacted = file_size(&path);
    database.compact().unwrap();
    let compacted = file_size(&path);
    eprintln!(
        "SIZE redb uncompacted={:.2}MiB compacted={:.2}MiB",
        mib(uncompacted),
        mib(compacted)
    );
    b.iter(|| black_box((uncompacted, compacted)));
}
