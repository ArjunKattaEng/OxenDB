//! Write-ahead log. See `docs/adr/0002-wal-and-recovery.md` for the design.

pub mod record;

pub use record::{TxnId, WalRecord};
