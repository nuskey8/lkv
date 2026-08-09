#![feature(test)]

extern crate test;

use lkv::{Database, DatabaseOptions};
use std::io::Write;
use std::path::Path;
use test::{Bencher, black_box};

const SMALL_KEYS: usize = 200_000;
const CHURN_KEYS: usize = 50_000;
const CHURN_ROUNDS: usize = 10;
const MANY_BATCH_KEYS: usize = 10_000;
const MANY_BATCH_SIZE: usize = 10;
const LARGE_VALUES: usize = 128;
const LARGE_VALUE_SIZE: usize = 1024 * 1024;
const LARGE_WRITE_CHUNK: usize = 256 * 1024;

fn options() -> DatabaseOptions {
    DatabaseOptions::default().with_overlay_memory_limit(usize::MAX)
}

fn create(path: &Path) -> Database {
    Database::create_with_options(path, options()).unwrap()
}

fn key(index: usize) -> [u8; 16] {
    let mut key = [0; 16];
    key[..8].copy_from_slice(&(index as u64).to_le_bytes());
    key[8..].copy_from_slice(&(!(index as u64)).to_le_bytes());
    key
}

fn put_small_entries(database: &mut Database, count: usize, generation: usize) {
    let mut transaction = database.begin_write().unwrap();
    for index in 0..count {
        transaction
            .put(key(index), [(index ^ generation) as u8; 64])
            .unwrap();
    }
    transaction.commit().unwrap();
}

fn put_small_batches(database: &mut Database, count: usize, batch_size: usize) {
    for start in (0..count).step_by(batch_size) {
        let mut transaction = database.begin_write().unwrap();
        for index in start..(start + batch_size).min(count) {
            transaction.put(key(index), [index as u8; 64]).unwrap();
        }
        transaction.commit().unwrap();
    }
}

fn bench_reopen(b: &mut Bencher, path: &Path) {
    b.iter(|| {
        let database = Database::open_with_options(path, options()).unwrap();
        black_box(database.get(key(0)).unwrap());
        black_box(database);
    });
}

#[bench]
fn reopen_compacted_200k(b: &mut Bencher) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("database.lkv");
    let mut database = create(&path);
    put_small_entries(&mut database, SMALL_KEYS, 0);
    database.vacuum().unwrap();
    drop(database);
    bench_reopen(b, &path);
}

#[bench]
fn reopen_overlay_200k(b: &mut Bencher) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("database.lkv");
    let mut database = create(&path);
    put_small_entries(&mut database, SMALL_KEYS, 0);
    drop(database);
    bench_reopen(b, &path);
}

#[bench]
fn reopen_churn_50k_x10(b: &mut Bencher) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("database.lkv");
    let mut database = create(&path);
    for generation in 0..CHURN_ROUNDS {
        put_small_entries(&mut database, CHURN_KEYS, generation);
    }
    drop(database);
    bench_reopen(b, &path);
}

#[bench]
fn reopen_many_batches_10k_by_10(b: &mut Bencher) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("database.lkv");
    let mut database = create(&path);
    put_small_batches(&mut database, MANY_BATCH_KEYS, MANY_BATCH_SIZE);
    drop(database);
    bench_reopen(b, &path);
}

#[bench]
fn reopen_overlay_128x1m(b: &mut Bencher) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("database.lkv");
    let mut database = create(&path);
    let chunk = [0x5a; LARGE_WRITE_CHUNK];
    let mut transaction = database.begin_write().unwrap();
    for index in 0..LARGE_VALUES {
        transaction
            .put_reserved(key(index), LARGE_VALUE_SIZE, |reserved| {
                for _ in 0..LARGE_VALUE_SIZE / chunk.len() {
                    reserved.write_all(&chunk)?;
                }
                Ok(())
            })
            .unwrap();
    }
    transaction.commit().unwrap();
    drop(database);
    bench_reopen(b, &path);
}
