#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::*;
use libakuma_tls::download_file;
use libakuma::{uptime, fstat};

#[no_mangle]
pub extern "C" fn main() {
    let args: Vec<String> = args().map(|s| String::from(s)).collect();
    cmd_pkg(&args);
}

// ============================================================================
// Core `pkg install` Logic (Moved from paws)
// ============================================================================

fn cmd_pkg(args: &[String]) {
    if args.len() >= 2 && args[1] == "test-speed" {
        cmd_test_speed(args);
    } else if args.len() < 3 || args[1] != "install" {
        libakuma::println("Usage: pkg install [--streaming] <package>");
        libakuma::println("       pkg test-speed <url> <dest_path>");
        return;
    } else {
        let server = "10.0.2.2:8000";
        let install_args = args[2..].to_vec();

        for package in &install_args {
            libakuma::println(format!("Installing {}...", package).as_str());

            // 1. Try to download as a binary
            let bin_url = format!("http://{}/bin/{}", server, package);
            let bin_dest = format!("/bin/{}", package);

            if download_file(&bin_url, &bin_dest).is_ok() {
                libakuma::println(format!("Installed binary to {}", bin_dest).as_str());
                continue;
            }

            // 2. Fallback to archive (.tar.gz then .tar)
            let archive_url_gz = format!("http://{}/archives/{}.tar.gz", server, package);
            let archive_dest_gz = format!("/tmp/{}.tar.gz", package);
            let archive_url_raw = format!("http://{}/archives/{}.tar", server, package);
            let archive_dest_raw = format!("/tmp/{}.tar", package);

            let (archive_dest, is_gz) = if download_file(&archive_url_gz, &archive_dest_gz).is_ok() {
                (archive_dest_gz, true)
            } else if download_file(&archive_url_raw, &archive_dest_raw).is_ok() {
                (archive_dest_raw, false)
            } else {
                libakuma::println(format!("Failed to find package {} as binary or archive.", package).as_str());
                continue;
            };

            libakuma::println(format!("Extracting {} to /...", archive_dest).as_str());
            let tar_args = [
                String::from("tar"),
                String::from(if is_gz { "-xzvf" } else { "-xvf" }),
                archive_dest.clone(),
                String::from("-C"),
                String::from("/"),
            ];

            if execute_external_with_status(&tar_args) == 0 {
                libakuma::println("Successfully extracted archive.");
                let _ = unlink(&archive_dest);
            } else {
                libakuma::println("Failed to extract archive.");
            }
        }
    }
}

fn cmd_test_speed(args: &[String]) {
    if args.len() < 4 {
        libakuma::println("Usage: pkg test-speed <url> <dest_path>");
        return;
    }
    let url = &args[2];
    let dest_path = &args[3];

    libakuma::println(format!("Testing download speed for URL: {}", url).as_str());

    let start_time_us = uptime();
    match download_file(url, dest_path) {
        Ok(_) => {
            let end_time_us = uptime();
            let elapsed_us = end_time_us - start_time_us;
            let file_size_bytes = {
                let fd = open(dest_path, open_flags::O_RDONLY);
                if fd >= 0 {
                    let size = fstat(fd).map_or(0, |s| s.st_size as u64);
                    close(fd);
                    size
                } else {
                    0
                }
            };

            if elapsed_us > 0 {
                let speed_bps = (file_size_bytes as f64 * 8_000_000.0) / elapsed_us as f64;
                let speed_mbps = speed_bps / 1_000_000.0;
                libakuma::println(format!("Downloaded {} bytes in {} us. Speed: {:.2} Mbps", file_size_bytes, elapsed_us, speed_mbps).as_str());
            } else {
                libakuma::println(format!("Downloaded {} bytes in <1 us. Speed: N/A (too fast)", file_size_bytes).as_str());
            }
            let _ = unlink(dest_path);
        }
        Err(e) => {
            libakuma::println(format!("Error: {:?}", e).as_str());
        }
    }
}


// ============================================================================
// External Command Execution Helpers (Moved from paws)
// ============================================================================

fn find_bin(name: &str) -> String {
    if name.starts_with('/') || name.starts_with("./") {
        return String::from(name);
    }

    let paths = ["/bin", "/usr/bin"];
    for path in paths {
        let bin_path = format!("{}/{}", path, name);
        let fd = open(&bin_path, open_flags::O_RDONLY);
        if fd >= 0 {
            close(fd);
            return bin_path;
        }
    }
    
    // Default to /bin if not found, let spawn fail later
    format!("/bin/{}", name)
}

fn execute_external_with_status(args: &[String]) -> i32 {
    let path = find_bin(&args[0]);
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    
    print("pkg: executing ");
    print(&path);
    for arg in arg_refs.iter().skip(1) {
        print(" ");
        print(arg);
    }
    println("");

    if let Some(res) = spawn(&path, Some(&arg_refs)) {
        let status = stream_output(res.stdout_fd, res.pid);
        print("pkg: process ");
        print(&path);
        print(" exited with status ");
        print_dec(status as usize);
        println("");
        return status;
    } else {
        print("pkg: command not found: ");
        println(&args[0]);
        return -1;
    }
}

fn stream_output(stdout_fd: u32, pid: u32) -> i32 {
    let mut buf = [0u8; 4096];
    loop {
        let n = read_fd(stdout_fd as i32, &mut buf);
        if n > 0 { write(fd::STDOUT, &buf[..n as usize]); }
        
        if let Some((_, exit_code)) = waitpid(pid) {
            // Drain any remaining output
            loop {
                let n = read_fd(stdout_fd as i32, &mut buf);
                if n <= 0 { break; }
                write(fd::STDOUT, &buf[..n as usize]);
            }
            return exit_code;
        }
    }
}
