//! oxenDB: an embeddable SQL database engine.
//!
//! oxenDB is in early development. There is no SQL interface yet; what
//! exists is a crash-safe, transactional page store that the SQL layers will
//! be built on.
//!
//! # Example
//!
//! ```
//! use oxendb::storage::page::PageType;
//! use oxendb::{Database, Options};
//!
//! # fn main() -> oxendb::Result<()> {
//! # let dir = std::env::temp_dir().join(format!("oxendb-doc-{}", std::process::id()));
//! # std::fs::create_dir_all(&dir).unwrap();
//! # let path = dir.join("example.oxen");
//! let db = Database::open(&path, Options::default())?;
//!
//! // Write transactions are atomic and durable once `commit` returns.
//! let mut txn = db.begin_write()?;
//! let id = txn.allocate_page(PageType::Heap);
//! txn.page_mut(id)?.payload_mut()[..5].copy_from_slice(b"hello");
//! txn.commit()?;
//!
//! // Read transactions see a consistent snapshot of committed data.
//! let read = db.begin_read()?;
//! let greeting = read.read_page(id, |page| page.payload()[..5].to_vec())?;
//! assert_eq!(greeting, b"hello");
//! drop(read);
//!
//! db.close()?;
//! # std::fs::remove_dir_all(&dir).unwrap();
//! # Ok(())
//! # }
//! ```
//!
//! # Layout
//!
//! The crate is organized by subsystem. Lower layers (storage) know nothing
//! about higher layers ([`db`], and later catalog, SQL, execution).

pub mod db;
pub mod error;
pub mod storage;

#[cfg(test)]
mod test_util;

pub use db::{Database, Options};
pub use error::{Error, Result};
