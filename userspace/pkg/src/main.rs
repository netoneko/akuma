#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::*;

#[no_mangle]
pub extern "C" fn main() {
    let args: Vec<String> = args().map(String::from).collect();
    cmd_pkg(&args);
}

use libakuma_tls::download_file;
// ============================================================================
// Core `pkg install` Logic (Moved from paws)
// ============================================================================

fn cmd_pkg(args: &[String]) {
    if args.len() < 3 || args[1] != "install" {
        println("Usage: pkg install <package>");
        return;
    }
    
    let server = "10.0.2.2:8000";
    
    for package in &args[2..] {
        println(format!("Installing {}...", package).as_str());
        
        // 1. Try to download as a binary
        let bin_url = format!("http://{}/bin/{}", server, package);
        let bin_dest = format!("/bin/{}", package);
        
        if download_file(&bin_url, &bin_dest).is_ok() {
             println(format!("Successfully installed binary to {}", bin_dest).as_str());
             continue;
        }

        // 2. Fallback to downloading as an archive
        let archive_url_gz = format!("http://{}/archives/{}.tar.gz", server, package);
        let archive_dest_gz = format!("/tmp/{}.tar.gz", package);
        let archive_url_raw = format!("http://{}/archives/{}.tar", server, package);
        let archive_dest_raw = format!("/tmp/{}.tar", package);
        
        let (_archive_url, archive_dest, is_gz) = if download_file(&archive_url_gz, &archive_dest_gz).is_ok() {
            (archive_url_gz, archive_dest_gz, true)
        } else if download_file(&archive_url_raw, &archive_dest_raw).is_ok() {
            (archive_url_raw, archive_dest_raw, false)
        } else {
            println(format!("Failed to find package {} as binary or archive.", package).as_str());
            continue;
        };

        println(format!("Extracting {} to /...", archive_dest).as_str());
        let mut tar_args = Vec::new();
        tar_args.push(String::from("tar"));
        tar_args.push(String::from(if is_gz { "-xzvf" } else { "-xvf" }));
        tar_args.push(archive_dest.clone());
        tar_args.push(String::from("-C"));
        tar_args.push(String::from("/"));

        if execute_external_with_status(&tar_args) == 0 {
            println("Successfully extracted archive.");
            let _ = unlink(&archive_dest);
        } else {
            println("Failed to extract archive.");
        }
    }
}



// ============================================================================
// External Command Execution Helpers (Moved from paws)
// ============================================================================

fn find_bin(name: &str) -> String {
    if name.starts_with('/') || name.starts_with("./") {
        String::from(name)
    } else {
        format!("/bin/{}", name)
    }
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
    let mut buf = [0u8; 1024];
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
        sleep_ms(5);
    }
}
