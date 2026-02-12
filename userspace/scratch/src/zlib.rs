//! Zlib compression/decompression utilities
//!
//! Git stores objects compressed with zlib (deflate).

use alloc::vec::Vec;

use miniz_oxide::inflate::stream::{inflate, InflateState};
use miniz_oxide::{DataFormat, MZFlush, MZStatus};

use crate::error::{Error, Result};

/// Decompress zlib-compressed data
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    miniz_oxide::inflate::decompress_to_vec_zlib(data)
        .map_err(|_| Error::decompress())
}

/// Decompress only the beginning of a zlib stream to extract headers.
/// Returns (bytes_consumed, bytes_written)
pub fn decompress_header(data: &[u8], out: &mut [u8]) -> Result<(usize, usize)> {
    let mut state = InflateState::new_boxed(DataFormat::Zlib);
    let result = inflate(&mut state, data, out, MZFlush::None);
    match result.status {
        Ok(_) => Ok((result.bytes_consumed, result.bytes_written)),
        Err(_) => Err(Error::decompress()),
    }
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
/// Uses the streaming inflate API to get exact byte counts.
pub fn decompress_with_consumed(data: &[u8]) -> Result<(Vec<u8>, usize)> {
    // Use heap-allocated state since InflateState is large (~32KB)
    let mut state = InflateState::new_boxed(DataFormat::Zlib);

    // Start small — `data` may be the entire remaining pack file,
    // not just this object's compressed data.
    let mut output = Vec::with_capacity(8192);
    let mut total_consumed = 0usize;
    let mut total_written = 0usize;

    loop {
        // Grow output buffer with doubling strategy (like Vec):
        // starts at 8KB, doubles each time, so large objects need few iterations.
        let available = output.len() - total_written;
        if available < 4096 {
            let growth = output.len().max(4096);
            output.resize(total_written + growth, 0);
        }

        let input_slice = &data[total_consumed..];
        let output_slice = &mut output[total_written..];

        let result = inflate(&mut state, input_slice, output_slice, MZFlush::None);

        total_consumed += result.bytes_consumed;
        total_written += result.bytes_written;

        match result.status {
            Ok(MZStatus::Ok) => {
                if result.bytes_consumed == 0 && result.bytes_written == 0 {
                    return Err(Error::decompress());
                }
            }
            Ok(MZStatus::StreamEnd) => {
                output.truncate(total_written);
                return Ok((output, total_consumed));
            }
            Ok(MZStatus::NeedDict) | Err(_) => {
                return Err(Error::decompress());
            }
        }
    }
}
