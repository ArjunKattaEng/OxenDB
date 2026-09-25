//! CRC32C throughput. Every page read, page write, WAL record, and recovery
//! step checksums 4 KiB, so this sits on every hot path.
//!
//! Run with `cargo bench -p oxendb --bench checksum`.

use std::hint::black_box;
use std::time::Instant;

use oxendb::storage::checksum::crc32c;

fn main() {
    for size in [64usize, 4096, 1 << 20] {
        let data: Vec<u8> = (0..size).map(|i| (i * 131 % 251) as u8).collect();
        // Aim for roughly 1 GiB of input per size, at least 1,000 calls.
        let iterations = ((1usize << 30) / size).max(1_000);
        let start = Instant::now();
        let mut acc = 0u32;
        for _ in 0..iterations {
            acc ^= crc32c(black_box(&data));
        }
        let elapsed = start.elapsed();
        black_box(acc);
        let gib_per_sec = (size * iterations) as f64 / elapsed.as_secs_f64() / (1u64 << 30) as f64;
        let per_call = elapsed / iterations as u32;
        println!("crc32c {size:>8} bytes  {gib_per_sec:>7.2} GiB/s  {per_call:>9.1?} per call");
    }
}
