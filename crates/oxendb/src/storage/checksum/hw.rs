//! CRC32C using dedicated CPU instructions: `crc32c*` on ARMv8 (the `crc`
//! feature) and `crc32` on x86-64 (SSE4.2).
//!
//! # Why `unsafe`
//!
//! The instructions are only reachable through functions compiled with
//! `#[target_feature]`. Calling such a function on a CPU without the feature
//! is undefined behavior, so the call is `unsafe`. The safety argument is the
//! same at every call site: the feature was detected at runtime immediately
//! before the call. That is the only unsafe operation in this module.
//!
//! Both paths are tested against the portable implementation in the parent
//! module. The x86-64 path can be run on Apple Silicon under Rosetta 2:
//! `cargo test --target x86_64-apple-darwin checksum`.

#![allow(unsafe_code)]

/// Updates `crc` with `bytes` using hardware instructions, or returns
/// `None` if this CPU does not support them.
pub(super) fn update(crc: u32, bytes: &[u8]) -> Option<u32> {
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("crc") {
        // SAFETY: the `crc` feature was detected on this CPU just above.
        return Some(unsafe { update_aarch64(crc, bytes) });
    }
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("sse4.2") {
        // SAFETY: SSE4.2 was detected on this CPU just above.
        return Some(unsafe { update_x86_64(crc, bytes) });
    }
    let _ = (crc, bytes);
    None
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "crc")]
fn update_aarch64(mut crc: u32, bytes: &[u8]) -> u32 {
    use std::arch::aarch64::{__crc32cb, __crc32cd};
    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        let word = u64::from_le_bytes(chunk.try_into().expect("chunk is 8 bytes"));
        crc = __crc32cd(crc, word);
    }
    for &byte in chunks.remainder() {
        crc = __crc32cb(crc, byte);
    }
    crc
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
fn update_x86_64(crc: u32, bytes: &[u8]) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};
    let mut crc = u64::from(crc);
    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        let word = u64::from_le_bytes(chunk.try_into().expect("chunk is 8 bytes"));
        crc = _mm_crc32_u64(crc, word);
    }
    // The 64-bit instruction zero-extends its 32-bit result.
    let mut crc = crc as u32;
    for &byte in chunks.remainder() {
        crc = _mm_crc32_u8(crc, byte);
    }
    crc
}
