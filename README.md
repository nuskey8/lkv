# lkv

![CI](https://github.com/nuskey8/lkv/actions/workflows/ci.yml/badge.svg)
[![Crates.io](https://img.shields.io/crates/v/lkv.svg)](https://crates.io/crates/lkv)
[![Documentation](https://docs.rs/lkv/badge.svg)](https://docs.rs/lkv)
[![License](https://img.shields.io/crates/l/lkv)](https://crates.io/crates/lkv)

A lightweight and fast embedded key-value store for Rust.

## Overview

lkv is a lightweight and fast embedded database implemented in Rust. It is designed specifically for read performance based on hash tables and memory efficiency, featuring faster lookups than LMDB, sled, redb, and others.

To maintain structural simplicity and read performance, lkv comes with a very limited set of features. Here is what lkv supports:

- Writing and reading arbitrary byte sequences `[u8]` as Key/Value
- Fast, unordered scans
- Fast and zero-copy lookups
- Transactions
- Snapshots
- Explicit compaction

On the other hand, the following features are not supported:

- Reading and writing from multiple processes
- Multiple writers
- Advanced queries such as ranged or prefix queries
- Automatic compaction

Due to its performance characteristics, it is suitable for lightweight configuration persistence or managing master data that rarely changes. Conversely, using it as a general-purpose database with frequent updates is not recommended.

Much of lkv's design is inspired by LinkedIn's [PalDB](https://github.com/linkedin/paldb) and [Bitcask](http://github.com/basho/bitcask). In addition, some ideas are based on [FASTER](https://github.com/microsoft/faster).

For details, refer to [docs/design.md](docs/design.md).

## Installation

```sh
cargo add lkv
```

## Quick Start

```rust
use lkv::{Database, Result};

fn main() -> Result<()> {
    let mut db = Database::create("./example.lkv")?;

    let mut write = db.begin_write()?;
    write.put("name", "lkv")?;
    write.commit()?;

    let read = db.begin_read()?;
    assert_eq!(read.get("name")?, Some(b"lkv".as_slice()));
    for item in read.iter()? {
        let (key, value) = item?;
        println!("{} = {}",
            String::from_utf8_lossy(key),
            String::from_utf8_lossy(value));
    }
    drop(read);

    Ok(())
}
```

## Snapshots

When writing to the database using `WriteTransaction` in lkv, you cannot perform reads at the same time. If you want to read from the database during a write, you can create a snapshot with `snapshot()` and read through it.

```rust
let snapshot = db.snapshot()?;
let old_value = snapshot.get("key")?;
for item in snapshot.iter()? {
    let (key, value) = item?;
}
```

## In-Memory

It is also possible to use lkv as an in-memory database.

```rust
let mut db = Database::memory();
```

The API is identical to that of a regular database, but it operates in memory without creating a file.

## Benchmark

| DB                | Bulk 100k (ms) | Write 1 (ms) | Write 1k (ms) | Read 100k (ms) | Delete 1 (ms) | Size (MiB) | Size compacted (MiB) |
| ----------------- | -------------: | -----------: | ------------: | -------------: | ------------: | ---------: | -------------------: |
| lkv               |          57.50 |         4.38 |          5.92 |           3.10 |          4.32 |      29.17 |                 9.44 |
| redb              |         145.95 |         4.83 |          7.20 |          37.51 |          4.92 |     128.50 |                16.70 |
| LMDB (heed)       |          39.94 |         4.84 |          6.07 |          34.84 |          4.92 |      36.13 |                 9.10 |
| RocksDB           |          45.21 |         4.64 |          6.86 |          75.14 |          4.67 |      10.07 |                10.07 |
| Fjall             |         162.57 |         4.53 |          6.50 |          69.10 |          4.62 |      73.80 |                  N/A |
| sled              |         658.69 |         4.63 |         14.59 |          48.92 |          5.54 |      80.18 |                  N/A |
| SQLite (rusqlite) |         116.60 |         0.33 |          1.40 |         522.41 |          0.41 |      21.39 |                10.51 |
| jammdb            |         117.96 |         4.88 |          8.92 |          57.59 |          5.02 |      64.50 |                  N/A |

## License

[MIT](LICENSE)
