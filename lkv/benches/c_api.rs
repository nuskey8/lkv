#![feature(test)]

extern crate test;

use lkv::ffi::*;
use std::ptr;
use std::sync::mpsc;
use std::thread;
use test::{Bencher, black_box};

const KEY: &[u8] = b"benchmark-key";
const LARGE_VALUE_SIZE: usize = 1024 * 1024;
const PARALLEL_READERS: usize = 4;
const READS_PER_ROUND: usize = 1_000;

fn database_with_value(value: &[u8]) -> *mut lkv_database {
    let mut database = ptr::null_mut();
    // SAFETY: all pointers refer to live Rust values for the duration of each call.
    unsafe {
        assert_eq!(lkv_database_open_memory(ptr::null(), &mut database), LKV_OK);
        assert_eq!(
            lkv_database_put(
                database,
                KEY.as_ptr(),
                KEY.len(),
                value.as_ptr(),
                value.len(),
            ),
            LKV_OK
        );
    }
    database
}

fn snapshot(database: *const lkv_database) -> *mut lkv_snapshot {
    let mut snapshot = ptr::null_mut();
    // SAFETY: database is a live handle and output points to writable storage.
    unsafe { assert_eq!(lkv_snapshot_create(database, &mut snapshot), LKV_OK) };
    snapshot
}

#[bench]
fn database_get_ref_64b(b: &mut Bencher) {
    let value = [0x5a; 64];
    let database = database_with_value(&value);
    let mut output = ptr::null();
    let mut output_len = 0;
    b.iter(|| {
        // SAFETY: the handle remains live and no mutation runs during the benchmark.
        let status = unsafe {
            lkv_database_get_ref(
                database,
                KEY.as_ptr(),
                KEY.len(),
                &mut output,
                &mut output_len,
            )
        };
        assert_eq!(status, LKV_OK);
        black_box((output, output_len));
    });
    // SAFETY: ownership of the live handle is returned exactly once.
    unsafe { assert_eq!(lkv_database_close(database), LKV_OK) };
}

#[bench]
fn database_get_ref_1m(b: &mut Bencher) {
    let value = vec![0x5a; LARGE_VALUE_SIZE];
    let database = database_with_value(&value);
    let mut output = ptr::null();
    let mut output_len = 0;
    b.iter(|| {
        // SAFETY: the handle remains live and no mutation runs during the benchmark.
        let status = unsafe {
            lkv_database_get_ref(
                database,
                KEY.as_ptr(),
                KEY.len(),
                &mut output,
                &mut output_len,
            )
        };
        assert_eq!(status, LKV_OK);
        black_box((output, output_len));
    });
    // SAFETY: ownership of the live handle is returned exactly once.
    unsafe { assert_eq!(lkv_database_close(database), LKV_OK) };
}

#[bench]
fn snapshot_get_ref_64b(b: &mut Bencher) {
    let value = [0x5a; 64];
    let database = database_with_value(&value);
    let snapshot = snapshot(database);
    let mut output = ptr::null();
    let mut output_len = 0;
    b.iter(|| {
        // SAFETY: the snapshot and output slots remain live for the benchmark.
        let status = unsafe {
            lkv_snapshot_get_ref(
                snapshot,
                KEY.as_ptr(),
                KEY.len(),
                &mut output,
                &mut output_len,
            )
        };
        assert_eq!(status, LKV_OK);
        black_box((output, output_len));
    });
    // SAFETY: ownership of both live handles is returned exactly once.
    unsafe {
        assert_eq!(lkv_snapshot_close(snapshot), LKV_OK);
        assert_eq!(lkv_database_close(database), LKV_OK);
    }
}

#[bench]
fn snapshot_get_ref_1m(b: &mut Bencher) {
    let value = vec![0x5a; LARGE_VALUE_SIZE];
    let database = database_with_value(&value);
    let snapshot = snapshot(database);
    let mut output = ptr::null();
    let mut output_len = 0;
    b.iter(|| {
        // SAFETY: the snapshot and output slots remain live for the benchmark.
        let status = unsafe {
            lkv_snapshot_get_ref(
                snapshot,
                KEY.as_ptr(),
                KEY.len(),
                &mut output,
                &mut output_len,
            )
        };
        assert_eq!(status, LKV_OK);
        black_box((output, output_len));
    });
    // SAFETY: ownership of both live handles is returned exactly once.
    unsafe {
        assert_eq!(lkv_snapshot_close(snapshot), LKV_OK);
        assert_eq!(lkv_database_close(database), LKV_OK);
    }
}

#[bench]
fn parallel_database_get_ref_4x1000(b: &mut Bencher) {
    let value = [0x5a; 64];
    let database = database_with_value(&value);
    let database_address = database as usize;
    let (done_tx, done_rx) = mpsc::channel();
    let mut starts = Vec::with_capacity(PARALLEL_READERS);
    let mut workers = Vec::with_capacity(PARALLEL_READERS);
    for _ in 0..PARALLEL_READERS {
        let (start_tx, start_rx) = mpsc::channel();
        starts.push(start_tx);
        let done_tx = done_tx.clone();
        workers.push(thread::spawn(move || {
            let database = database_address as *const lkv_database;
            let mut output = ptr::null();
            let mut output_len = 0;
            while start_rx.recv().is_ok() {
                for _ in 0..READS_PER_ROUND {
                    // SAFETY: the database outlives every worker and no writer runs.
                    let status = unsafe {
                        lkv_database_get_ref(
                            database,
                            KEY.as_ptr(),
                            KEY.len(),
                            &mut output,
                            &mut output_len,
                        )
                    };
                    assert_eq!(status, LKV_OK);
                }
                black_box((output, output_len));
                done_tx.send(()).unwrap();
            }
        }));
    }
    drop(done_tx);

    b.iter(|| {
        for start in &starts {
            start.send(()).unwrap();
        }
        for _ in 0..PARALLEL_READERS {
            done_rx.recv().unwrap();
        }
    });

    drop(starts);
    for worker in workers {
        worker.join().unwrap();
    }
    // SAFETY: all worker calls completed before ownership is returned.
    unsafe { assert_eq!(lkv_database_close(database), LKV_OK) };
}
