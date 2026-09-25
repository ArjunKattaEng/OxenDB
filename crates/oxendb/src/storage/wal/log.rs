//! The WAL file: a header followed by a sequence of records.
//!
//! # Header layout
//!
//! | Offset | Size | Field                                          |
//! |--------|------|------------------------------------------------|
//! | 0      | 4    | CRC32C of bytes `4..24`                        |
//! | 4      | 8    | Magic bytes `b"oxenWAL\0"`                     |
//! | 12     | 4    | Format version                                 |
//! | 16     | 8    | Sequence number of the first record            |
//!
//! Records follow immediately, see [`super::record`].

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::Path;

use super::record::{DecodeError, DecodedRecord, WalRecord};
use crate::error::{Error, Result};
use crate::storage::checksum::crc32c;
use crate::storage::disk::sync_parent_dir;

/// Identifies a file as an oxenDB write-ahead log.
pub const WAL_MAGIC: [u8; 8] = *b"oxenWAL\0";

/// WAL format version written by this build.
pub const WAL_FORMAT_VERSION: u32 = 1;

/// Size of the WAL file header.
pub const WAL_HEADER_SIZE: usize = 24;

/// What was found after the last valid record when opening a WAL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscardedTail {
    /// Number of bytes removed from the end of the file.
    pub bytes: u64,
    /// Why the first discarded byte range was not a valid record.
    pub reason: String,
}

/// Result of opening an existing WAL.
#[derive(Debug)]
pub struct OpenedWal {
    /// The log, positioned to append after the last valid record.
    pub wal: Wal,
    /// Every valid record in the log, in order.
    pub records: Vec<DecodedRecord>,
    /// Present if bytes after the last valid record were discarded.
    pub discarded: Option<DiscardedTail>,
}

/// An append-only write-ahead log file.
#[derive(Debug)]
pub struct Wal {
    file: File,
    /// Byte offset just past the last durable record.
    durable_len: u64,
    /// Encoded records appended but not yet flushed.
    pending: Vec<u8>,
    next_seq: u64,
    /// Set after a failed write or fsync. The state of the file is then
    /// unknown, so the log refuses further use until it is reopened.
    poisoned: bool,
}

impl Wal {
    /// Creates a new, empty WAL. Fails if `path` already exists.
    pub fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all_at(&encode_header(0), 0)?;
        file.sync_all()?;
        sync_parent_dir(path)?;
        Ok(Wal {
            file,
            durable_len: WAL_HEADER_SIZE as u64,
            pending: Vec::new(),
            next_seq: 0,
            poisoned: false,
        })
    }

    /// Opens an existing WAL and reads every valid record.
    ///
    /// Reading stops at the first record that is incomplete, fails
    /// validation, or has an unexpected sequence number. Everything from that
    /// point on is the remains of an interrupted write; it is truncated so
    /// later appends are readable.
    pub fn open(path: &Path) -> Result<OpenedWal> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)?;
        let first_seq = decode_header(&contents)?;

        let mut records = Vec::new();
        let mut offset = WAL_HEADER_SIZE;
        let mut next_seq = first_seq;
        let mut stop_reason = None;
        while offset < contents.len() {
            match WalRecord::decode(&contents[offset..]) {
                Ok(decoded) if decoded.seq == next_seq => {
                    offset += decoded.len;
                    next_seq += 1;
                    records.push(decoded);
                }
                Ok(decoded) => {
                    stop_reason = Some(format!(
                        "expected sequence number {next_seq}, found {}",
                        decoded.seq
                    ));
                    break;
                }
                Err(DecodeError::Incomplete) => {
                    stop_reason = Some("incomplete record".to_string());
                    break;
                }
                Err(DecodeError::Invalid(reason)) => {
                    stop_reason = Some(reason);
                    break;
                }
            }
        }

        let discarded = stop_reason.map(|reason| DiscardedTail {
            bytes: (contents.len() - offset) as u64,
            reason,
        });
        if discarded.is_some() {
            file.set_len(offset as u64)?;
            file.sync_all()?;
        }
        let wal = Wal {
            file,
            durable_len: offset as u64,
            pending: Vec::new(),
            next_seq,
            poisoned: false,
        };
        Ok(OpenedWal {
            wal,
            records,
            discarded,
        })
    }

    /// Buffers a record for the next [`Wal::flush`] and returns its sequence
    /// number. The record is not durable until the flush succeeds.
    pub fn append(&mut self, record: &WalRecord) -> Result<u64> {
        self.check_poisoned()?;
        let seq = self.next_seq;
        record.encode(seq, &mut self.pending);
        self.next_seq += 1;
        Ok(seq)
    }

    /// Writes buffered records and fsyncs. When this returns `Ok`, every
    /// appended record is durable.
    ///
    /// On failure the log is poisoned: retrying an fsync after an error is
    /// not safe on common operating systems, because the kernel may already
    /// have dropped the dirty data. Reopen the database to recover.
    pub fn flush(&mut self) -> Result<()> {
        self.check_poisoned()?;
        if self.pending.is_empty() {
            return Ok(());
        }
        let result = self
            .file
            .write_all_at(&self.pending, self.durable_len)
            .and_then(|()| self.file.sync_data());
        if let Err(err) = result {
            self.poisoned = true;
            return Err(err.into());
        }
        self.durable_len += self.pending.len() as u64;
        self.pending.clear();
        Ok(())
    }

    /// Removes every record, keeping sequence numbers increasing.
    ///
    /// Only call this once every change in the log is durable in the data
    /// file, or those changes will be lost.
    pub fn reset(&mut self) -> Result<()> {
        self.check_poisoned()?;
        if !self.pending.is_empty() {
            return Err(Error::InvalidArgument(
                "cannot reset a WAL with unflushed records".into(),
            ));
        }
        // Either order of these two writes is crash-safe: a stale header with
        // no records is empty, and a new header in front of old records
        // rejects them by sequence number.
        let result = self
            .file
            .set_len(WAL_HEADER_SIZE as u64)
            .and_then(|()| self.file.write_all_at(&encode_header(self.next_seq), 0))
            .and_then(|()| self.file.sync_all());
        if let Err(err) = result {
            self.poisoned = true;
            return Err(err.into());
        }
        self.durable_len = WAL_HEADER_SIZE as u64;
        Ok(())
    }

    /// Sequence number the next appended record will get.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Size of the durable part of the log in bytes, including the header.
    pub fn durable_len(&self) -> u64 {
        self.durable_len
    }

    fn check_poisoned(&self) -> Result<()> {
        if self.poisoned {
            return Err(Error::Poisoned(
                "an earlier WAL write failed; reopen the database to recover".into(),
            ));
        }
        Ok(())
    }
}

fn encode_header(first_seq: u64) -> [u8; WAL_HEADER_SIZE] {
    let mut header = [0u8; WAL_HEADER_SIZE];
    header[4..12].copy_from_slice(&WAL_MAGIC);
    header[12..16].copy_from_slice(&WAL_FORMAT_VERSION.to_le_bytes());
    header[16..24].copy_from_slice(&first_seq.to_le_bytes());
    let checksum = crc32c(&header[4..]);
    header[0..4].copy_from_slice(&checksum.to_le_bytes());
    header
}

/// Validates the header and returns the first record's sequence number.
fn decode_header(contents: &[u8]) -> Result<u64> {
    if contents.len() < WAL_HEADER_SIZE || contents[4..12] != WAL_MAGIC {
        return Err(Error::UnsupportedFormat("not an oxenDB WAL file".into()));
    }
    let header = &contents[..WAL_HEADER_SIZE];
    let stored = u32::from_le_bytes(header[0..4].try_into().unwrap());
    if stored != crc32c(&header[4..]) {
        return Err(Error::corruption("WAL header checksum mismatch"));
    }
    let version = u32::from_le_bytes(header[12..16].try_into().unwrap());
    if version != WAL_FORMAT_VERSION {
        return Err(Error::UnsupportedFormat(format!(
            "WAL format version {version}, this build supports version {WAL_FORMAT_VERSION}"
        )));
    }
    Ok(u64::from_le_bytes(header[16..24].try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::page::{Page, PageId, PageType};
    use crate::storage::wal::TxnId;
    use crate::test_util::TempDir;

    fn image(txn: u64, page_id: u64, fill: u8) -> WalRecord {
        let mut page = Page::new(PageId(page_id), PageType::Heap);
        page.payload_mut().fill(fill);
        page.seal();
        WalRecord::PageImage {
            txn: TxnId(txn),
            page_id: PageId(page_id),
            page,
        }
    }

    fn commit(txn: u64) -> WalRecord {
        WalRecord::Commit { txn: TxnId(txn) }
    }

    fn records_of(opened: &OpenedWal) -> Vec<WalRecord> {
        opened.records.iter().map(|r| r.record.clone()).collect()
    }

    #[test]
    fn flushed_records_survive_reopen() {
        let dir = TempDir::new();
        let path = dir.path().join("db.oxen-wal");
        let written = vec![image(1, 1, 1), image(1, 2, 2), commit(1)];
        {
            let mut wal = Wal::create(&path).unwrap();
            for record in &written {
                wal.append(record).unwrap();
            }
            wal.flush().unwrap();
        }
        let opened = Wal::open(&path).unwrap();
        assert_eq!(records_of(&opened), written);
        assert_eq!(opened.discarded, None);
        assert_eq!(opened.wal.next_seq(), 3);
    }

    #[test]
    fn unflushed_records_are_lost() {
        let dir = TempDir::new();
        let path = dir.path().join("db.oxen-wal");
        {
            let mut wal = Wal::create(&path).unwrap();
            wal.append(&commit(1)).unwrap();
            wal.flush().unwrap();
            wal.append(&commit(2)).unwrap();
        }
        assert_eq!(records_of(&Wal::open(&path).unwrap()), vec![commit(1)]);
    }

    #[test]
    fn torn_tail_is_discarded_at_every_cut() {
        let dir = TempDir::new();
        let path = dir.path().join("db.oxen-wal");
        {
            let mut wal = Wal::create(&path).unwrap();
            wal.append(&commit(1)).unwrap();
            wal.flush().unwrap();
            wal.append(&image(2, 5, 9)).unwrap();
            wal.flush().unwrap();
        }
        let full = std::fs::read(&path).unwrap();
        let first_end = WAL_HEADER_SIZE + {
            let mut buf = Vec::new();
            commit(1).encode(0, &mut buf);
            buf.len()
        };
        // Every cut inside the record header, then a sample of the body.
        // The record decoder's own tests cover every byte offset in memory;
        // this checks the file-level truncation, which fsyncs per case.
        let cuts = (first_end..first_end + 64)
            .chain((first_end + 64..full.len()).step_by(97))
            .chain(full.len() - 8..full.len());
        for cut in cuts {
            std::fs::write(&path, &full[..cut]).unwrap();
            let opened = Wal::open(&path).unwrap();
            assert_eq!(records_of(&opened), vec![commit(1)], "cut at {cut}");
            assert_eq!(opened.discarded.is_some(), cut > first_end, "cut at {cut}");
            assert_eq!(std::fs::metadata(&path).unwrap().len(), first_end as u64);
        }
    }

    #[test]
    fn appends_after_torn_tail_are_readable() {
        let dir = TempDir::new();
        let path = dir.path().join("db.oxen-wal");
        {
            let mut wal = Wal::create(&path).unwrap();
            wal.append(&commit(1)).unwrap();
            wal.flush().unwrap();
        }
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(&[0xEE; 100]);
        std::fs::write(&path, &bytes).unwrap();

        {
            let mut opened = Wal::open(&path).unwrap();
            assert_eq!(opened.discarded.as_ref().unwrap().bytes, 100);
            opened.wal.append(&commit(2)).unwrap();
            opened.wal.flush().unwrap();
        }
        assert_eq!(
            records_of(&Wal::open(&path).unwrap()),
            vec![commit(1), commit(2)]
        );
    }

    #[test]
    fn reset_empties_log_and_continues_sequence() {
        let dir = TempDir::new();
        let path = dir.path().join("db.oxen-wal");
        {
            let mut wal = Wal::create(&path).unwrap();
            wal.append(&commit(1)).unwrap();
            wal.append(&commit(2)).unwrap();
            wal.flush().unwrap();
            wal.reset().unwrap();
            assert_eq!(wal.durable_len(), WAL_HEADER_SIZE as u64);
            wal.append(&commit(3)).unwrap();
            wal.flush().unwrap();
        }
        let opened = Wal::open(&path).unwrap();
        assert_eq!(records_of(&opened), vec![commit(3)]);
        assert_eq!(opened.records[0].seq, 2);
    }

    #[test]
    fn stale_records_behind_new_header_are_ignored() {
        // Simulates a crash during reset after the header was rewritten but
        // before the file was truncated.
        let dir = TempDir::new();
        let path = dir.path().join("db.oxen-wal");
        {
            let mut wal = Wal::create(&path).unwrap();
            wal.append(&commit(1)).unwrap();
            wal.flush().unwrap();
        }
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[..WAL_HEADER_SIZE].copy_from_slice(&encode_header(1));
        std::fs::write(&path, &bytes).unwrap();

        let opened = Wal::open(&path).unwrap();
        assert!(opened.records.is_empty());
        assert_eq!(opened.wal.next_seq(), 1);
    }

    #[test]
    fn reset_with_pending_records_is_refused() {
        let dir = TempDir::new();
        let mut wal = Wal::create(&dir.path().join("db.oxen-wal")).unwrap();
        wal.append(&commit(1)).unwrap();
        assert!(matches!(wal.reset(), Err(Error::InvalidArgument(_))));
    }

    #[test]
    fn rejects_bad_headers() {
        let dir = TempDir::new();
        let path = dir.path().join("db.oxen-wal");
        std::fs::write(&path, b"not a wal").unwrap();
        assert!(matches!(Wal::open(&path), Err(Error::UnsupportedFormat(_))));

        let mut header = encode_header(0);
        header[20] ^= 1;
        std::fs::write(&path, header).unwrap();
        assert!(matches!(Wal::open(&path), Err(Error::Corruption(_))));
    }
}
