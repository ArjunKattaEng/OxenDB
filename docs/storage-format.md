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

## Not yet specified

The heap page payload layout, the WAL file, and free-space tracking are not
implemented yet. They will be documented here as they land.
