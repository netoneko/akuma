#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use libakuma::{self, exit, args, eprintln, println, open, close, read_fd, write_fd, mkdir_p, open_flags};
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
            for c in arg.chars().skip(1) {
                match c {
                    'z' => gzip = true,
                    'x' => extract = true,
                    'v' => verbose = true,
                    'f' => {
                        if i + 1 < args_vec.len() {
                            archive_file = Some(args_vec[i + 1].clone());
                            i += 1;
                        } else {
                            eprintln("tar: option requires an argument -- f");
                            exit(1);
                        }
                    }
                    'C' => {
                        if i + 1 < args_vec.len() {
                            target_dir = args_vec[i + 1].clone();
                            i += 1;
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

    match untar_gz(&archive_path, &target_dir, verbose) {
        Ok(_) => {},
        Err(e) => {
            eprintln(&format!("tar: error: {:?}", e));
            exit(1);
        }
    }
}

#[derive(Debug)]
enum TarError {
    LibakumaError(i32),
    GzipDecompressionError(&'static str),
    TarArchiveError(&'static str),
}

fn read_file_to_vec(path: &str) -> Result<Vec<u8>, TarError> {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        return Err(TarError::LibakumaError(fd));
    }

    let mut buffer = Vec::new();
    let mut temp_buf = [0u8; 4096];

    loop {
        let bytes_read = read_fd(fd, &mut temp_buf);
        if bytes_read < 0 {
            close(fd);
            return Err(TarError::LibakumaError(bytes_read as i32));
        }
        if bytes_read == 0 {
            break;
        }
        buffer.extend_from_slice(&temp_buf[..bytes_read as usize]);
    }
    close(fd);
    Ok(buffer)
}

fn untar_gz(archive_path: &str, target_dir: &str, verbose: bool) -> Result<(), TarError> {
    let compressed_data = read_file_to_vec(archive_path)?;

    let decompressed_data = inflate::decompress_to_vec(&compressed_data)
        .map_err(|_| TarError::GzipDecompressionError("decompression failed"))?;

    let archive = TarArchiveRef::new(&decompressed_data)
        .map_err(|_| TarError::TarArchiveError("invalid tar archive"))?;

    for entry in archive.entries() {
        let filename = entry.filename();
        let path = filename.as_str().unwrap_or("unknown");
        
        // Skip empty paths or entries without data
        if path.is_empty() || path == "." {
            continue;
        }

        // Clean path and prepend target_dir
        let mut full_path = String::from(target_dir);
        if !full_path.ends_with('/') && !path.starts_with('/') {
            full_path.push('/');
        }
        full_path.push_str(path);

        if verbose {
            println(&format!("{}", path));
        }

        // Handle directories (tar entries for directories usually end in /)
        if path.ends_with('/') {
            mkdir_p(&full_path);
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
            write_fd(fd, data);
            close(fd);
        } else if verbose {
            eprintln(&format!("tar: failed to create file {}: errno {}", full_path, -fd));
        }
    }

    Ok(())
}
