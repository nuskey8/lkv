use crate::common::{
    BATCH_SIZE, INITIAL_ENTRIES, directory_size, file_size, key, mib, read_indices, temp_dir, value,
};
use ::heed::{CompactionOption, Database, Env, EnvOpenOptions, types::Bytes};
use std::fs::File;
use test::{Bencher, black_box};

fn create(path: &std::path::Path) -> (Env, Database<Bytes, Bytes>) {
    let env = unsafe {
        EnvOpenOptions::new()
            .map_size(1 << 30)
            .max_dbs(1)
            .open(path)
            .unwrap()
    };
    let mut transaction = env.write_txn().unwrap();
    let database = env.create_database(&mut transaction, None).unwrap();
    transaction.commit().unwrap();
    (env, database)
}

fn populate(env: &Env, database: Database<Bytes, Bytes>) {
    let mut transaction = env.write_txn().unwrap();
    for index in 0..INITIAL_ENTRIES {
        database
            .put(
                &mut transaction,
                key(index).as_slice(),
                value(index, 0).as_slice(),
            )
            .unwrap();
    }
    transaction.commit().unwrap();
}

fn populated() -> (tempfile::TempDir, Env, Database<Bytes, Bytes>) {
    let dir = temp_dir();
    let (env, database) = create(dir.path());
    populate(&env, database);
    (dir, env, database)
}

#[bench]
fn bulk_load_100k(b: &mut Bencher) {
    let mut dirs = Vec::new();
    b.iter(|| {
        let dir = temp_dir();
        let (env, database) = create(dir.path());
        populate(&env, database);
        drop(env);
        dirs.push(dir);
    });
    black_box(dirs);
}

#[bench]
fn write_single_sync(b: &mut Bencher) {
    let (_dir, env, database) = populated();
    let mut index = INITIAL_ENTRIES;
    b.iter(|| {
        let mut transaction = env.write_txn().unwrap();
        database
            .put(
                &mut transaction,
                key(index).as_slice(),
                value(index, 1).as_slice(),
            )
            .unwrap();
        transaction.commit().unwrap();
        index += 1;
    });
}

#[bench]
fn write_batch_1000_sync(b: &mut Bencher) {
    let (_dir, env, database) = populated();
    let mut next = INITIAL_ENTRIES;
    b.iter(|| {
        let mut transaction = env.write_txn().unwrap();
        for index in next..next + BATCH_SIZE {
            database
                .put(
                    &mut transaction,
                    key(index).as_slice(),
                    value(index, 1).as_slice(),
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        next += BATCH_SIZE;
    });
}

#[bench]
fn read_random_100k(b: &mut Bencher) {
    let (_dir, env, database) = populated();
    let indices = read_indices();
    let transaction = env.read_txn().unwrap();
    b.iter(|| {
        let checksum = indices.iter().fold(0u64, |sum, &index| {
            let value = database
                .get(&transaction, key(index).as_slice())
                .unwrap()
                .unwrap();
            sum.wrapping_add(u64::from(value[0]))
        });
        black_box(checksum)
    });
}

#[bench]
fn delete_single_sync(b: &mut Bencher) {
    let (_dir, env, database) = populated();
    let mut index = 0;
    b.iter(|| {
        assert!(index < INITIAL_ENTRIES, "delete fixture exhausted");
        let mut transaction = env.write_txn().unwrap();
        assert!(
            database
                .delete(&mut transaction, key(index).as_slice())
                .unwrap()
        );
        transaction.commit().unwrap();
        index += 1;
    });
}

#[bench]
fn size_uncompacted_compacted(b: &mut Bencher) {
    let (dir, env, database) = populated();
    let mut transaction = env.write_txn().unwrap();
    for index in 0..INITIAL_ENTRIES {
        database
            .put(
                &mut transaction,
                key(index).as_slice(),
                value(index, 1).as_slice(),
            )
            .unwrap();
    }
    for index in 0..INITIAL_ENTRIES / 2 {
        database
            .delete(&mut transaction, key(index).as_slice())
            .unwrap();
    }
    transaction.commit().unwrap();
    let uncompacted = directory_size(dir.path());
    let compacted_path = dir.path().join("compacted.mdb");
    let mut compacted_file = File::create(&compacted_path).unwrap();
    env.copy_to_file(&mut compacted_file, CompactionOption::Enabled)
        .unwrap();
    compacted_file.sync_all().unwrap();
    let compacted = file_size(&compacted_path) + file_size(&dir.path().join("lock.mdb"));
    eprintln!(
        "SIZE lmdb uncompacted={:.2}MiB compacted={:.2}MiB",
        mib(uncompacted),
        mib(compacted)
    );
    b.iter(|| black_box((uncompacted, compacted)));
}
