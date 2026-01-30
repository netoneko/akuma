//! Zlib compression/decompression utilities
//!
//! Git stores objects compressed with zlib (deflate).

use alloc::vec::Vec;

use crate::error::{Error, Result};

/// Decompress zlib-compressed data
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    miniz_oxide::inflate::decompress_to_vec_zlib(data)
        .map_err(|_| Error::decompress())
}

/// Compress data with zlib
pub fn compress(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
}

/// Decompress with a size hint for better allocation
pub fn decompress_with_size(data: &[u8], _expected_size: usize) -> Result<Vec<u8>> {
    // Just use the standard decompress function
    // The allocator will handle sizing
    decompress(data)
}
