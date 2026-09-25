//! Write-ahead log. See `docs/adr/0002-wal-and-recovery.md` for the design.

pub mod log;
pub mod record;

pub use log::{OpenedWal, Wal};
pub use record::{TxnId, WalRecord};
