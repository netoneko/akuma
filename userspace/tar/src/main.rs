#![no_std]
#![no_main] // This tells rustc that this crate does not use the standard main entry point.
// #![feature(naked_functions)] // Stable since 1.88.0
// #![feature(asm_const)] // Stable since 1.82.0
// #![feature(const_pin)] // Stable since 1.84.0
// #![feature(const_mut_refs)] // Stable since 1.83.0
// #![feature(const_btree_new)] // Replaced by const_btree_len (partially stabilized 1.66.0)
// #![feature(const_for)] // Stable since 1.79.0
// #![feature(const_heap_allocated)] // This feature is unknown

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

// Use specific libakuma functions
use libakuma::{self, exit, args, eprintln, open, close, read_fd};
use libakuma::open_flags; // For open flags constants
use miniz_oxide::inflate;
use tar_no_std::TarArchiveRef; // Use TarArchiveRef

#[no_mangle]
fn _start() -> ! {
    main();
    exit(0);
}

fn main() {
    let args_iter = args();
    // Convert libakuma::args() iterator to Vec<String>
    let mut args_vec: Vec<String> = Vec::new();
    for arg in args_iter {
        args_vec.push(String::from(arg));
    }

    let mut extract = false;
    let mut gzip = false;
    let mut verbose = false;
    let mut archive_file: Option<String> = None;

    let mut i = 1; // Skip program name
    while i < args_vec.len() {
        let arg = &args_vec[i];
        if arg.starts_with('-') {
            for c in arg.chars().skip(1) {
                match c {
                    'z' => gzip = true,
                    'x' => extract = true,
                    'v' => verbose = true,
                    'f' => {
                        if i + 1 < args_vec.len() {
                            archive_file = Some(args_vec[i + 1].clone());
                            i += 1; // Consume the next argument as the file name
                        } else {
                            eprintln("tar: option requires an argument -- f");
                            exit(1);
                        }
                    }
                    _ => {
                        eprintln(&alloc::format!("tar: invalid option -- '{}'", c));
                        exit(1);
                    }
                }
            }
        } else if archive_file.is_none() {
            archive_file = Some(arg.clone());
        } else {
            eprintln(&alloc::format!("tar: extra operand '{}'", arg));
            exit(1);
        }
        i += 1;
    }

    if !extract {
        eprintln("tar: only extraction (-x) is supported for now.");
        exit(1);
    }

    if !gzip {
        eprintln("tar: only gzipped archives (-z) are supported for now.");
        exit(1);
    }

    let archive_path = match archive_file {
        Some(path) => path,
        None => {
            eprintln("tar: archive file not specified.");
            exit(1);
        }
    };

    match untar_gz(&archive_path, verbose) {
        Ok(_) => {},
        Err(e) => {
            eprintln(&alloc::format!("tar: error: {:?}", e)); // Changed to {:?}
            exit(1);
        }
    }
}

// Custom Error type
#[derive(Debug)]
enum TarError {
    LibakumaError(i32),
    GzipDecompressionError(&'static str),
    TarArchiveError(&'static str),
    Other(&'static str),
}

impl ToString for TarError {
    fn to_string(&self) -> String {
        alloc::format!("{:?}", self)
    }
}

// Helper to read the entire file into a Vec<u8>
fn read_file_to_vec(path: &str) -> Result<Vec<u8>, TarError> {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        return Err(TarError::LibakumaError(fd));
    }

    let mut buffer = Vec::new();
    let mut temp_buf = [0u8; 4096]; // Read in chunks

    loop {
        let bytes_read = read_fd(fd, &mut temp_buf);
        if bytes_read < 0 {
            close(fd);
            return Err(TarError::LibakumaError(bytes_read as i32));
        }
        if bytes_read == 0 {
            break; // EOF
        }
        buffer.extend_from_slice(&temp_buf[..bytes_read as usize]);
    }
    close(fd);
    Ok(buffer)
}

fn untar_gz(archive_path: &str, verbose: bool) -> Result<(), TarError> {
    let compressed_data = read_file_to_vec(archive_path)?;

    let decompressed_data = inflate::decompress_to_vec(&compressed_data)
        .map_err(|_| TarError::GzipDecompressionError("decompression failed"))?; // Fixed DecompressError field

    let archive = TarArchiveRef::new(&decompressed_data)
        .map_err(|_| TarError::TarArchiveError("invalid tar archive"))?;

    for entry in archive.entries() { // Iterate directly over ArchiveEntry
        let path = entry.filename(); // filename() returns TarFormatString<256> directly
        let path_str = path.as_str().unwrap_or("invalid filename"); // Convert TarFormatString to &str

        if verbose {
            eprintln(&alloc::format!("{}", path_str));
        }
        
        // This unpack_in expects a path to create files.
        // `tar-no-std`'s `Entry::unpack_in` internally uses `std::fs` calls,
        // which are not available in our `no_std` Akuma environment.
        // TODO: Implement custom filesystem for tar-no_std extraction.
        // For now, we will just print the file names.
        
        // entry.unpack_in(".")?; // This will fail because it uses std::fs
    }

    Ok(())
}
