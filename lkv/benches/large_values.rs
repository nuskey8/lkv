#![feature(test)]

extern crate test;

use lkv::{Database, DatabaseOptions};
use std::io::Write;
use test::{Bencher, black_box};

const LARGE_VALUE_SIZE: usize = 64 * 1024 * 1024;
const WRITE_CHUNK_SIZE: usize = 256 * 1024;

fn options() -> DatabaseOptions {
    DatabaseOptions::default().with_overlay_memory_limit(usize::MAX)
}

#[bench]
fn put_64m_stage(b: &mut Bencher) {
    let value = vec![0x5a; LARGE_VALUE_SIZE];
    let mut database = Database::memory_with_options(options()).unwrap();
    b.bytes = LARGE_VALUE_SIZE as u64;
    b.iter(|| {
        let mut transaction = database.begin_write().unwrap();
        transaction.put(b"value", black_box(&value)).unwrap();
        black_box(transaction.get(b"value").unwrap());
        transaction.abort();
    });
}

#[bench]
fn put_reserved_64m_stage(b: &mut Bencher) {
    let directory = tempfile::tempdir().unwrap();
    let mut database =
        Database::create_with_options(directory.path().join("database.lkv"), options()).unwrap();
    let chunk = [0xa5; WRITE_CHUNK_SIZE];
    b.bytes = LARGE_VALUE_SIZE as u64;
    b.iter(|| {
        let mut transaction = database.begin_write().unwrap();
        transaction
            .put_reserved(b"value", LARGE_VALUE_SIZE, |reserved| {
                let mut remaining = LARGE_VALUE_SIZE;
                while remaining > 0 {
                    let len = remaining.min(chunk.len());
                    reserved.write_all(black_box(&chunk[..len]))?;
                    remaining -= len;
                }
                Ok(())
            })
            .unwrap();
        black_box(transaction.get(b"value").unwrap());
        transaction.abort();
    });
}

#[bench]
fn get_64m_mapped(b: &mut Bencher) {
    let directory = tempfile::tempdir().unwrap();
    let mut database =
        Database::create_with_options(directory.path().join("database.lkv"), options()).unwrap();
    let chunk = [0x3c; WRITE_CHUNK_SIZE];
    let mut transaction = database.begin_write().unwrap();
    transaction
        .put_reserved(b"value", LARGE_VALUE_SIZE, |reserved| {
            let mut remaining = LARGE_VALUE_SIZE;
            while remaining > 0 {
                let len = remaining.min(chunk.len());
                reserved.write_all(&chunk[..len])?;
                remaining -= len;
            }
            Ok(())
        })
        .unwrap();
    transaction.commit().unwrap();

    b.iter(|| {
        let value = database.get(black_box(b"value")).unwrap().unwrap();
        black_box((value.as_ptr(), value.len()));
    });
}
