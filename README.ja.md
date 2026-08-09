# lkv

![CI](https://github.com/nuskey8/lkv/actions/workflows/ci.yml/badge.svg)
[![Crates.io](https://img.shields.io/crates/v/lkv.svg)](https://crates.io/crates/lkv)
[![Documentation](https://docs.rs/lkv/badge.svg)](https://docs.rs/lkv)
[![License](https://img.shields.io/crates/l/lkv)](https://crates.io/crates/lkv)

A lightweight and fast embedded key-value store for Rust.

## 概要

lkvはRust実装の軽量かつ高速な組み込みDBです。ハッシュテーブルをベースとした読み取り性能や省メモリに特化した設計になっており、LMDBやsled、redbなどよりも高速なルックアップを特徴としています。

lkvは構造のシンプルさと読み取り時のパフォーマンスを保つため、非常に少ない機能のみを備えています。以下はlkvがサポートするものです。

- 任意のバイト列`[u8]`をKey/Valueとした書き込み・読み取り
- 順序未定義の高速スキャン
- 高速かつゼロコピーなルックアップ
- トランザクション
- スナップショット
- 明示的なcompaction

一方で、以下のような機能はサポート外です。

- 複数プロセスからの読み書き
- 複数writer
- 範囲付き、プレフィクスなどの高度なクエリ
- 自動compaction

その性能特性から、軽量な設定の永続化やほとんど変更されることのないマスタデータの管理などに向いています。一方、頻繁に更新される汎用的なDBとしての利用は推奨されません。

lkvの設計の大部分はLinkedinの[PalDB](https://github.com/linkedin/paldb)および[Bitcask](http://github.com/basho/bitcask)にインスパイアされています。また、一部のアイデアは[FASTER](https://github.com/microsoft/faster)を参考としています。

詳細は[docs/design.md](docs/design.md)を参照してください。

## インストール

```sh
cargo add lkv
```

## クイックスタート

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

## スナップショット

lkvでは`WriteTransaction`を用いてDBへの書き込みを行う場合、同時に読み取りを行うことはできません。書き込み中にDBを読み取りたい場合、`snapshot()`でスナップショットを作成し、これを介して読み取ることが可能です。

```rust
let snapshot = db.snapshot()?;
let old_value = snapshot.get("key")?;
for item in snapshot.iter()? {
    let (key, value) = item?;
}
```

## インメモリ

lkvをインメモリデータベースとして利用することも可能です。

```rust
let mut db = Database::memory();
```

APIは通常のデータベースと同一ですが、ファイルを作成せずにメモリ上で動作します。

## ベンチマーク

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

## ライセンス

[MIT](LICENSE)
