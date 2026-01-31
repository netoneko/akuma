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
    use libakuma::print;
    
    print("scratch: decompress_with_consumed input_len=");
    print_num(data.len());
    print("\n");
    
    // Use heap-allocated state since InflateState is large (~32KB)
    let mut state = InflateState::new_boxed(DataFormat::Zlib);
    
    print("scratch: InflateState created\n");
    
    // Start with a reasonable output buffer size
    let mut output = Vec::with_capacity(data.len() * 2);
    let mut total_consumed = 0usize;
    let mut total_written = 0usize;
    let mut iterations = 0usize;
    
    loop {
        iterations += 1;
        if iterations > 1000 {
            print("scratch: too many iterations\n");
            return Err(Error::decompress());
        }
        
        // Extend output buffer if needed
        if output.len() < total_written + 4096 {
            output.resize(total_written + 4096, 0);
        }
        
        let input_slice = &data[total_consumed..];
        let output_slice = &mut output[total_written..];
        
        let result = inflate(&mut state, input_slice, output_slice, MZFlush::None);
        
        total_consumed += result.bytes_consumed;
        total_written += result.bytes_written;
        
        match result.status {
            Ok(MZStatus::Ok) => {
                // Need more output space or more input
                if result.bytes_consumed == 0 && result.bytes_written == 0 {
                    // No progress - need more data
                    print("scratch: decompress no progress\n");
                    return Err(Error::decompress());
                }
                // Continue decompressing
            }
            Ok(MZStatus::StreamEnd) => {
                // Done! Truncate output to actual size
                output.truncate(total_written);
                print("scratch: decompress done consumed=");
                print_num(total_consumed);
                print(" output=");
                print_num(total_written);
                print("\n");
                return Ok((output, total_consumed));
            }
            Ok(MZStatus::NeedDict) => {
                // Preset dictionary required - not supported
                print("scratch: decompress need dict\n");
                return Err(Error::decompress());
            }
            Err(_) => {
                print("scratch: decompress error\n");
                return Err(Error::decompress());
            }
        }
    }
}

fn print_num(n: usize) {
    use libakuma::print;
    if n == 0 {
        print("0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 0;
    let mut val = n;
    while val > 0 {
        buf[i] = b'0' + (val % 10) as u8;
        val /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        let s = core::str::from_utf8(&buf[i..i+1]).unwrap();
        print(s);
    }
}
