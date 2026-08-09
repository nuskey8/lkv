#![feature(test)]

extern crate test;

mod common;

#[path = "comparison/fjall.rs"]
mod fjall;
#[path = "comparison/hashmap.rs"]
mod hashmap;
#[path = "comparison/heed.rs"]
mod heed;
#[path = "comparison/jammdb.rs"]
mod jammdb;
#[path = "comparison/lkv.rs"]
mod lkv;
#[path = "comparison/redb.rs"]
mod redb;
#[path = "comparison/rocksdb.rs"]
mod rocksdb;
#[path = "comparison/sled.rs"]
mod sled;
#[path = "comparison/sqlite.rs"]
mod sqlite;
