use super::*;
use crate::CorruptionKind;
use crate::format::segment::{
    self, BASE_HEADER, CHECKSUM_BLOCK_SIZE, MAX_KEY_SIZE, SLOT_SIZE, read_base_header,
    validate_base,
};
use crate::format::superblock::{FORMAT_VERSION, HEADER_SIZE, SUPERBLOCK_SIZE};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

trait DatabaseTestExt {
    fn put(&mut self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<()>;
    fn delete(&mut self, key: impl AsRef<[u8]>) -> Result<()>;
}

impl DatabaseTestExt for Database {
    fn put(&mut self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<()> {
        let mut transaction = self.begin_write()?;
        transaction.put(key, value)?;
        transaction.commit()
    }

    fn delete(&mut self, key: impl AsRef<[u8]>) -> Result<()> {
        let mut transaction = self.begin_write()?;
        transaction.delete(key)?;
        transaction.commit()
    }
}

struct FailAfter<'a> {
    file: &'a mut Storage,
    remaining: usize,
}

impl Write for FailAfter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let written = self.file.write(&bytes[..bytes.len().min(self.remaining)])?;
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn temp_path() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "lkv-test-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&root).expect("test directory creation must succeed");
    root.join("database.lkv")
}

fn temp_dir() -> PathBuf {
    let path = temp_path();
    drop(Database::create(&path).expect("test database creation must succeed"));
    path
}

fn remove_test_database(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::other("test database has no parent directory"))?;
    fs::remove_dir_all(parent)?;
    Ok(())
}

mod limits;
mod maintenance;
mod recovery;
mod transactions;
mod verification;
