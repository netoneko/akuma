#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use libakuma::{self, exit, args, print, println, print_dec, print_hex, eprintln, open, close, read_fd, write_fd, mkdir_p, open_flags};
use miniz_oxide::inflate;
use tar_no_std::TarArchiveRef;

#[no_mangle]
fn _start() -> ! {
    main();
    exit(0);
}

fn main() {
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

    let raw_data = read_file_to_vec(archive_path)?;
    if verbose {
        print("tar: read ");
        print_dec(raw_data.len());
        println(" bytes");
    }

    let decompressed_data;
    let data_to_use = if gzip {
        decompressed_data = inflate::decompress_to_vec(&raw_data)
            .map_err(|_| TarError::GzipDecompressionError("decompression failed"))?;
        &decompressed_data
    } else {
        &raw_data
    };

    let archive = TarArchiveRef::new(data_to_use)
        .map_err(|_| TarError::TarArchiveError("invalid tar archive"))?;

    let mut entry_count = 0;
    for entry in archive.entries() {
        entry_count += 1;
        let filename = entry.filename();
        let mut path = filename.as_str().unwrap_or("unknown");
        
        // Skip empty paths or entries without data
        if path.is_empty() || path == "." {
            continue;
        }

        // Remove leading ./ if present
        if path.starts_with("./") {
            path = &path[2..];
        }
        
        if path.is_empty() {
            continue;
        }

        // Clean path and prepend target_dir
        let mut full_path = String::from(target_dir);
        if !full_path.ends_with('/') && !path.starts_with('/') {
            full_path.push('/');
        } else if full_path.ends_with('/') && path.starts_with('/') {
            // Avoid double slash
            full_path.pop();
        }
        full_path.push_str(path);

        if verbose {
            print("Extracting: ");
            print(path);
            print(" (");
            print_dec(entry.size());
            print(" bytes) -> ");
            println(&full_path);
        }

        // Handle directories (tar entries for directories usually end in /)
        if path.ends_with('/') {
            if !mkdir_p(&full_path) && verbose {
                eprintln(&format!("tar: warning: failed to create directory {}", full_path));
            }
            continue;
        }

        // Ensure parent directory exists
        if let Some(last_slash) = full_path.rfind('/') {
            let parent = &full_path[..last_slash];
            if !parent.is_empty() {
                mkdir_p(parent);
            }
        }

        // Write file data
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
