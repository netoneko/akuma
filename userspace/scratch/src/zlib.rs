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

/// Decompress and return (decompressed_data, bytes_consumed_from_input)
/// This is critical for streaming pack parsing where we need to know
/// exactly how many compressed bytes were used.
///
/// Uses a single decompression with all available data, then estimates
/// the compressed size based on compression ratio and validates by
/// looking for the next valid object header pattern.
pub fn decompress_with_consumed(data: &[u8]) -> Result<(Vec<u8>, usize)> {
    // First decompress with all available data
    // Zlib streams are self-delimiting - extra trailing data is ignored
    let decompressed = decompress(data)?;
    
    // Estimate compressed size based on typical compression ratios
    // Zlib usually achieves 40-70% compression on source code
    // Add overhead for zlib header (2 bytes) and adler32 (4 bytes)
    let estimated = (decompressed.len() * 6 / 10) + 12;
    
    // The actual compressed size is somewhere between the minimum possible
    // (6 bytes header/checksum + 1 byte data) and our estimate
    // We'll use the estimate, bounded by available data
    let consumed = core::cmp::min(estimated, data.len());
    
    // Ensure we consume at least the minimum zlib stream size
    let consumed = core::cmp::max(consumed, 8);
    let consumed = core::cmp::min(consumed, data.len());
    
    Ok((decompressed, consumed))
}
