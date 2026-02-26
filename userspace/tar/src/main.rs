#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::{self, exit, args, print, println, print_dec, eprintln, open, close, read_fd, write_fd, mkdir_p, open_flags};
use miniz_oxide::inflate;
use tar_no_std::TarArchiveRef;

#[no_mangle]
pub extern "C" fn main() {
    let args_iter = args();
    let mut args_vec: Vec<String> = Vec::new();
    for arg in args_iter {
        args_vec.push(String::from(arg));
    }

    let mut extract = false;
    let mut gzip = false;
    let mut verbose = false;
    let mut archive_file: Option<String> = None;
    let mut target_dir: String = String::from(".");

    let mut i = 1;
    while i < args_vec.len() {
        let arg = &args_vec[i];
        if arg.starts_with('-') {
            let mut stop_bundle = false;
            for (char_idx, c) in arg.chars().skip(1).enumerate() {
                if stop_bundle { break; }
                match c {
                    'z' => gzip = true,
                    'x' => extract = true,
                    'v' => verbose = true,
                    'f' => {
                        if char_idx + 2 < arg.len() {
                            // Filename is in the same bundle: -xfarchive.tar
                            archive_file = Some(String::from(&arg[char_idx + 2..]));
                            stop_bundle = true;
                        } else if i + 1 < args_vec.len() {
                            // Filename is next argument
                            archive_file = Some(args_vec[i + 1].clone());
                            i += 1;
                            stop_bundle = true;
                        } else {
                            eprintln("tar: option requires an argument -- f");
                            exit(1);
                        }
                    }
                    'C' => {
                        if char_idx + 2 < arg.len() {
                            // Path is in the same bundle
                            target_dir = String::from(&arg[char_idx + 2..]);
                            stop_bundle = true;
                        } else if i + 1 < args_vec.len() {
                            target_dir = args_vec[i + 1].clone();
                            i += 1;
                            stop_bundle = true;
                        } else {
                            eprintln("tar: option requires an argument -- C");
                            exit(1);
                        }
                    }
                    _ => {
                        eprintln(&format!("tar: invalid option -- '{}'", c));
                        exit(1);
                    }
                }
            }
        } else if archive_file.is_none() {
            archive_file = Some(arg.clone());
        } else {
            eprintln(&format!("tar: extra operand '{}'", arg));
            exit(1);
        }
        i += 1;
    }

    if !extract {
        eprintln("tar: only extraction (-x) is supported for now.");
        exit(1);
    }

    let archive_path = match archive_file {
        Some(path) => path,
        None => {
            eprintln("tar: archive file not specified.");
            exit(1);
        }
    };

    match untar(&archive_path, &target_dir, gzip, verbose) {
        Ok(_) => {},
        Err(e) => {
            match &e {
                TarError::LibakumaError(errno, path) => {
                    eprintln(&format!("tar: error: LibakumaError({}) for path '{}'", errno, path));
                }
                TarError::GzipDecompressionError(msg) => {
                    eprintln(&format!("tar: error: Gzip decompression failed: {}", msg));
                }
                TarError::TarArchiveError(msg) => {
                    eprintln(&format!("tar: error: Invalid tar archive: {}", msg));
                }
            }
            exit(1);
        }
    }
}

#[derive(Debug)]
enum TarError {
    LibakumaError(i32, String),
    GzipDecompressionError(&'static str),
    TarArchiveError(&'static str),
}

fn read_file_to_vec(path: &str) -> Result<Vec<u8>, TarError> {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        return Err(TarError::LibakumaError(fd, String::from(path)));
    }

    let mut buffer = Vec::new();
    let mut temp_buf = [0u8; 4096];

    loop {
        let bytes_read = read_fd(fd, &mut temp_buf);
        if bytes_read < 0 {
            close(fd);
            return Err(TarError::LibakumaError(bytes_read as i32, String::from(path)));
        }
        if bytes_read == 0 {
            break;
        }
        buffer.extend_from_slice(&temp_buf[..bytes_read as usize]);
    }
    close(fd);
    Ok(buffer)
}

fn untar(archive_path: &str, target_dir: &str, gzip: bool, verbose: bool) -> Result<(), TarError> {
    if verbose {
        print("tar: untar path='");
        print(archive_path);
        print("' target='");
        print(target_dir);
        print("' gzip=");
        print(if gzip { "true" } else { "false" });
        print(" verbose=");
        println(if verbose { "true" } else { "false" });
    }

    if gzip {
        untar_in_memory(archive_path, target_dir, verbose)
    } else {
        untar_streaming(archive_path, target_dir, verbose)
    }
}

/// Streaming tar extraction: reads 512-byte headers and file data directly
/// from the fd without loading the entire archive into memory.
fn untar_streaming(archive_path: &str, target_dir: &str, verbose: bool) -> Result<(), TarError> {
    let fd = open(archive_path, open_flags::O_RDONLY);
    if fd < 0 {
        return Err(TarError::LibakumaError(fd, String::from(archive_path)));
    }

    let mut header = [0u8; 512];
    let mut entry_count: usize = 0;
    let mut zero_blocks = 0;

    loop {
        if !read_exact(fd, &mut header) {
            break;
        }

        if header.iter().all(|&b| b == 0) {
            zero_blocks += 1;
            if zero_blocks >= 2 {
                break;
            }
            continue;
        }
        zero_blocks = 0;

        if !verify_header_checksum(&header) {
            eprintln("tar: warning: bad header checksum, stopping");
            break;
        }

        let path_raw = parse_tar_path(&header);
        let size = parse_octal(&header[124..136]);
        let typeflag = header[156];

        // Skip pax extended headers and GNU long name/link entries
        if typeflag == b'x' || typeflag == b'g' || typeflag == b'L' || typeflag == b'K' {
            read_skip(fd, padded_size(size));
            continue;
        }

        let mut path = path_raw.as_str();
        if path.is_empty() || path == "." {
            read_skip(fd, padded_size(size));
            continue;
        }
        if path.starts_with("./") {
            path = &path[2..];
        }
        if path.is_empty() {
            read_skip(fd, padded_size(size));
            continue;
        }

        let full_path = join_path(target_dir, path);
        entry_count += 1;

        // '5' = directory, or path ends with /
        if typeflag == b'5' || path.ends_with('/') {
            if verbose {
                print("Extracting: ");
                print(path);
                println("/");
            }
            if !mkdir_p(&full_path) && verbose {
                eprintln(&format!("tar: warning: failed to create directory {}", full_path));
            }
            read_skip(fd, padded_size(size));
            continue;
        }

        if verbose {
            print("Extracting: ");
            print(path);
            print(" (");
            print_dec(size);
            print(" bytes) -> ");
            println(&full_path);
        }

        // Ensure parent directory exists
        if let Some(last_slash) = full_path.rfind('/') {
            let parent = &full_path[..last_slash];
            if !parent.is_empty() {
                mkdir_p(parent);
            }
        }

        // Stream file data to disk in chunks
        let out_fd = open(&full_path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
        if out_fd < 0 {
            eprintln(&format!("tar: error: failed to create file {}: errno {}", full_path, -out_fd));
            read_skip(fd, padded_size(size));
            continue;
        }

        let mut remaining = size;
        let mut buf = [0u8; 4096];
        while remaining > 0 {
            let to_read = if remaining > buf.len() { buf.len() } else { remaining };
            let n = read_fd(fd, &mut buf[..to_read]);
            if n <= 0 {
                break;
            }
            let n = n as usize;
            let written = write_fd(out_fd, &buf[..n]);
            if written < 0 {
                eprintln(&format!("tar: error: write failed for {}: errno {}", full_path, -written));
                break;
            }
            remaining -= n;
        }
        close(out_fd);

        // Read past padding to next 512-byte boundary
        let padding = padded_size(size) - (size - remaining);
        if padding > 0 {
            read_skip(fd, padding);
        }
    }

    close(fd);

    print("tar: extracted ");
    print_dec(entry_count);
    println(" entries");

    Ok(())
}

/// In-memory tar extraction for gzip archives (decompression requires full data).
fn untar_in_memory(archive_path: &str, target_dir: &str, verbose: bool) -> Result<(), TarError> {
    let raw_data = read_file_to_vec(archive_path)?;
    if verbose {
        print("tar: read ");
        print_dec(raw_data.len());
        println(" bytes");
    }

    let decompressed_data = inflate::decompress_to_vec(&raw_data)
        .map_err(|_| TarError::GzipDecompressionError("decompression failed"))?;
    drop(raw_data);

    let archive = TarArchiveRef::new(&decompressed_data)
        .map_err(|_| TarError::TarArchiveError("invalid tar archive"))?;

    let mut entry_count = 0;
    for entry in archive.entries() {
        entry_count += 1;
        let filename = entry.filename();
        let mut path = filename.as_str().unwrap_or("unknown");

        if path.is_empty() || path == "." {
            continue;
        }
        if path.starts_with("./") {
            path = &path[2..];
        }
        if path.is_empty() {
            continue;
        }

        let full_path = join_path(target_dir, path);

        if verbose {
            print("Extracting: ");
            print(path);
            print(" (");
            print_dec(entry.size());
            print(" bytes) -> ");
            println(&full_path);
        }

        if path.ends_with('/') {
            if !mkdir_p(&full_path) && verbose {
                eprintln(&format!("tar: warning: failed to create directory {}", full_path));
            }
            continue;
        }

        if let Some(last_slash) = full_path.rfind('/') {
            let parent = &full_path[..last_slash];
            if !parent.is_empty() {
                mkdir_p(parent);
            }
        }

        let data = entry.data();
        let fd = open(&full_path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
        if fd >= 0 {
            let written = write_fd(fd, data);
            close(fd);
            if written < 0 {
                eprintln(&format!("tar: error: failed to write file {}: errno {}", full_path, -written));
            }
        } else {
            eprintln(&format!("tar: error: failed to create file {}: errno {}", full_path, -fd));
        }
    }

    print("tar: extracted ");
    print_dec(entry_count);
    println(" entries");

    Ok(())
}

// ============================================================================
// Tar format helpers
// ============================================================================

fn join_path(target_dir: &str, path: &str) -> String {
    let mut full_path = String::from(target_dir);
    if !full_path.ends_with('/') && !path.starts_with('/') {
        full_path.push('/');
    } else if full_path.ends_with('/') && path.starts_with('/') {
        full_path.pop();
    }
    full_path.push_str(path);
    full_path
}

/// Parse filename from tar header, using USTAR prefix only when magic is present.
fn parse_tar_path(header: &[u8; 512]) -> String {
    let name = extract_str(&header[0..100]);

    // Only use the prefix field if the USTAR magic is present at offset 257
    let magic = &header[257..263];
    let is_ustar = magic.starts_with(b"ustar\0");
    if is_ustar {
        let prefix = extract_str(&header[345..500]);
        if !prefix.is_empty() {
            return format!("{}/{}", prefix, name);
        }
    }
    name
}

/// Verify the tar header checksum to detect garbage/misaligned reads.
fn verify_header_checksum(header: &[u8; 512]) -> bool {
    let stored = parse_octal(&header[148..156]);

    // Checksum is computed with the checksum field treated as spaces (0x20)
    let mut sum: u32 = 0;
    for (i, &b) in header.iter().enumerate() {
        if (148..156).contains(&i) {
            sum += 0x20;
        } else {
            sum += b as u32;
        }
    }

    // Some implementations use signed byte arithmetic
    let mut signed_sum: u32 = 0;
    for (i, &b) in header.iter().enumerate() {
        if (148..156).contains(&i) {
            signed_sum += 0x20;
        } else {
            signed_sum += (b as i8) as u32;
        }
    }

    sum as usize == stored || signed_sum as usize == stored
}

/// Extract a null-terminated string from a byte slice.
fn extract_str(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from(core::str::from_utf8(&bytes[..end]).unwrap_or(""))
}

/// Parse an octal size field from tar header bytes.
fn parse_octal(bytes: &[u8]) -> usize {
    let s = core::str::from_utf8(bytes).unwrap_or("");
    let s = s.trim_matches(|c: char| c == '\0' || c == ' ');
    usize::from_str_radix(s, 8).unwrap_or(0)
}

/// Round up to next 512-byte block boundary.
fn padded_size(size: usize) -> usize {
    (size + 511) / 512 * 512
}

/// Read exactly `buf.len()` bytes from fd. Returns false on EOF/error.
fn read_exact(fd: i32, buf: &mut [u8]) -> bool {
    let mut offset = 0;
    while offset < buf.len() {
        let n = read_fd(fd, &mut buf[offset..]);
        if n <= 0 {
            return false;
        }
        offset += n as usize;
    }
    true
}

/// Skip `n` bytes by reading and discarding (no lseek dependency).
fn read_skip(fd: i32, mut n: usize) {
    let mut buf = [0u8; 4096];
    while n > 0 {
        let to_read = if n > buf.len() { buf.len() } else { n };
        let got = read_fd(fd, &mut buf[..to_read]);
        if got <= 0 {
            break;
        }
        n -= got as usize;
    }
}
