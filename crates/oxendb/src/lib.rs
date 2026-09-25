//! oxenDB: an embeddable SQL database engine.
//!
//! The crate is organized by subsystem. Lower layers (storage) know nothing
//! about higher layers (catalog, SQL, execution).

pub mod error;
pub mod storage;

pub use error::{Error, Result};
