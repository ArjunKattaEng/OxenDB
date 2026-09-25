# Storage format

This describes the on-disk format as of format version 1. It is not stable
yet: until oxenDB reaches 1.0, the format may change between releases, and
the format version in the file header will be bumped when it does.

All integers are little-endian.

## Database file

A database is a single file made of fixed 4096-byte pages. Page `n` starts at
byte offset `n * 4096`.

```
+-----------+-----------+-----------+-----
|  page 0   |  page 1   |  page 2   | ...
|  header   |  data     |  data     |
+-----------+-----------+-----------+-----
```

The page count in the header is only updated at checkpoint and during
recovery, so the file is often longer than the header says: pages from
recent transactions are written back before the next checkpoint updates the
count. Between checkpoints the WAL holds the current header (see
[ADR 0002](adr/0002-wal-and-recovery.md)). A file *shorter* than the header
says is treated as corruption.

## File header (page 0)

| Offset | Size | Field                                  |
|--------|------|----------------------------------------|
| 0      | 4    | CRC32C of bytes `4..4096`              |
| 4      | 8    | Magic: `oxenDB\0\0`                    |
| 12     | 4    | Format version (currently `1`)         |
| 16     | 4    | Page size (currently `4096`)           |
| 20     | 8    | Page count, including the header page  |
| 28     | ...  | Reserved, zero                         |

On open, checks happen in this order: magic, checksum, format version, page
size. A wrong magic number is reported as "not an oxenDB database file" so a
wrong path gives a clear error instead of a checksum failure.

## Data page header

Every page after page 0 starts with a 24-byte header:

| Offset | Size | Field                                         |
|--------|------|-----------------------------------------------|
| 0      | 4    | CRC32C of bytes `4..4096`                     |
| 4      | 1    | Page type (`1` = free, `2` = heap)            |
| 5      | 3    | Reserved, zero                                |
| 8      | 8    | Page id                                       |
| 16     | 8    | LSN of the last WAL record applied            |
| 24     | ...  | Payload, format depends on page type          |

Page type `0` is invalid on purpose, so an all-zero page never passes
validation. The stored page id lets a read detect that it got the wrong
page back (a misdirected write or read).

Every read verifies the checksum, page type, reserved bytes, and page id
before the page is handed to the rest of the engine.

## WAL file

The write-ahead log lives next to the database as `<database>-wal`. Its
design is explained in [ADR 0002](adr/0002-wal-and-recovery.md).

### Header (24 bytes)

| Offset | Size | Field                                  |
|--------|------|----------------------------------------|
| 0      | 4    | CRC32C of bytes `4..24`                |
| 4      | 8    | Magic: `oxenWAL\0`                     |
| 12     | 4    | WAL format version (currently `1`)     |
| 16     | 8    | Sequence number of the first record    |

### Records

Records follow the header back to back.

| Offset | Size | Field                                           |
|--------|------|-------------------------------------------------|
| 0      | 4    | Record length in bytes, including this field    |
| 4      | 4    | CRC32C of bytes `8..length`                     |
| 8      | 8    | Sequence number (previous record's + 1)         |
| 16     | 1    | Record type                                     |
| 17     | 8    | Transaction id                                  |
| 25     | ...  | Body                                            |

| Type | Name        | Body                                    | Length |
|------|-------------|-----------------------------------------|--------|
| 1    | `PageImage` | page id (8 bytes), full page (4096)     | 4129   |
| 2    | `Commit`    | none                                    | 25     |

Page id 0 in a `PageImage` is an image of the file header.

A reader stops at the first record that is incomplete, fails its checksum,
has an impossible length or type, or does not carry the expected sequence
number. Everything from there to the end of the file is discarded as the
remains of an interrupted write.

After a checkpoint the file is truncated back to its header, and the
header's first sequence number is set to the next unused one, so records
from before the checkpoint can never be mistaken for current ones.

## Not yet specified

The heap page payload layout and free-space tracking are not implemented
yet. They will be documented here as they land.
