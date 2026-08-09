//! ## lkv
//!
//! A lightweight and fast embedded key-value store for Rust.
//!
//! For more details, see the [crates.io](https://crates.io/crates/lkv).
//!
//! ## Examples
//!
//! ```no_run
//! use lkv::{Database, Result};
//!
//! fn main() -> Result<()> {
//!     let mut db = Database::create("./example.lkv")?;
//!     let mut write = db.begin_write()?;
//!     write.put("name", "lkv")?;
//!     write.commit()?;
//!
//!     let read = db.begin_read()?;
//!     assert_eq!(read.get("name")?, Some(b"lkv".as_slice()));
//!     for item in read.iter()? {
//!         let (key, value) = item?;
//!         println!("{} = {}",
//!             String::from_utf8_lossy(key),
//!             String::from_utf8_lossy(value));
//!     }
//!     drop(read);
//!
//!     Ok(())
//! }
//! ```

mod database;
mod error;
mod format;
mod options;

#[cfg(feature = "ffi")]
pub mod ffi;

pub use database::{
    Database, DatabaseStats, Entries, ReadTransaction, ReservedValue, Snapshot, WriteTransaction,
};
pub use error::{Corruption, CorruptionKind, Error, Result};
pub use options::{DatabaseOptions, VerificationMode};
