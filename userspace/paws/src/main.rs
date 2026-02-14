#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::*;
use libakuma::net::TcpStream;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    main();
    exit(0);
}

fn main() {
    println("paws v0.1.0 - Process Awareness & Workspace Shell");
    println("Type 'help' for available commands.");

    loop {
        print_prompt();
        let input = read_line();
        let trimmed = input.trim();
        
        if trimmed.is_empty() {
            if input.is_empty() && !input.is_empty() { // Placeholder for potential logic
            }
            // If input is truly empty (EOF), we should exit
            // but read_line currently returns "" on both empty and EOF
            // Let's check for EOF specifically.
            continue;
        }

        let args = parse_args(trimmed);
        if args.is_empty() {
            continue;
        }

        match args[0].as_str() {
            "exit" | "quit" => break,
            "help" => cmd_help(),
            "pwd" => println(getcwd()),
            "cd" => cmd_cd(&args),
            "ls" => cmd_ls(&args),
            "pkg" => cmd_pkg(&args),
            "cp" => cmd_cp(&args),
            "mv" => cmd_mv(&args),
            "rm" => cmd_rm(&args),
            "find" => cmd_find(&args),
            "grep" => cmd_grep(&args),
            "echo" => cmd_echo(&args),
            _ => execute_external(&args),
        }
    }
}

fn print_prompt() {
    print("paws ");
    print(getcwd());
    print(" # ");
}

fn read_line() -> String {
    let mut line = String::new();
    let mut buf = [0u8; 1];
    
    loop {
        // Use blocking poll instead of non-blocking read
        let n = poll_input_event(core::u64::MAX, &mut buf);
        
        if n < 0 {
            // Error
            break;
        }
        
        if n == 0 {
            // EOF (Ctrl+D)
            if line.is_empty() {
                println("exit");
                exit(0);
            }
            break;
        }
        
        let c = buf[0];
        if c == b'\n' || c == b'\r' {
            println("");
            break;
        } else if c == 8 || c == 127 {
            // Backspace
            if !line.is_empty() {
                line.pop();
                print("\x08 \x08"); // Erase on terminal
            }
        } else if c == 4 {
            // Ctrl+D
            if line.is_empty() {
                println("exit");
                exit(0);
            }
            // ignore if line not empty or handle as EOF
            break;
        } else if c >= 32 {
            line.push(c as char);
            let s = core::str::from_utf8(&buf).unwrap_or("");
            print(s);
        }
    }
    line
}

fn parse_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for c in input.chars() {
        if c == '"' {
            in_quotes = !in_quotes;
        } else if c.is_whitespace() && !in_quotes {
            if !current.is_empty() {
                args.push(current.clone());
                current.clear();
            }
        } else {
            current.push(c);
        }
    }

    if !current.is_empty() {
        args.push(current);
    }
    args
}

fn cmd_help() {
    println("Built-in commands:");
    println("  cd <dir>              Change directory");
    println("  pwd                   Print working directory");
    println("  ls [dir]              List directory contents");
    println("  cp <src> <dest>       Copy file");
    println("  mv <src> <dest>       Move/rename file");
    println("  rm <path>             Remove file");
    println("  find <path> [name]    Find files recursively");
    println("  grep <pat> <file>     Search for pattern in file");
    println("  echo [args...]        Print arguments");
    println("  pkg install <pkgs>    Install packages");
    println("  help                  Show this help");
    println("  exit                  Exit paws");
}

fn cmd_echo(args: &[String]) {
    for (i, arg) in args.iter().enumerate().skip(1) {
        if i > 1 {
            print(" ");
        }
        print(arg);
    }
    println("");
}

fn cmd_cd(args: &[String]) {
    let target = if args.len() < 2 {
        "/"
    } else {
        &args[1]
    };

    if chdir(target) != 0 {
        print("cd: failed to change directory to ");
        println(target);
    }
}

fn cmd_ls(args: &[String]) {
    let mut show_all = false;
    let mut path = ".";

    for arg in args.iter().skip(1) {
        if arg == "-a" {
            show_all = true;
        } else if !arg.starts_with('-') {
            path = arg;
        }
    }

    if let Some(reader) = read_dir(path) {
        let mut entries: Vec<DirEntryInfo> = reader.collect();
        // Sort entries alphabetically
        entries.sort_by(|a, b| a.name.cmp(&b.name));

        for entry in entries {
            if !show_all && entry.name.starts_with('.') && entry.name != "." && entry.name != ".." {
                // In most unix systems, . and .. are shown even without -a if they are the only ones?
                // Actually no, they are hidden. But we should check if they are returned.
                continue;
            }
            
            // Skip hidden files if not show_all
            if !show_all && entry.name.starts_with('.') {
                continue;
            }

            if entry.is_dir {
                print("\x1b[1;34m"); // Bold Blue
                print(&entry.name);
                print("\x1b[0m");
                print("/");
            } else {
                print(&entry.name);
            }
            print("  ");
        }
        println("");
    } else {
        print("ls: cannot access ");
        println(path);
    }
}

fn cmd_cp(args: &[String]) {
    if args.len() < 3 {
        println("Usage: cp <src> <dest>");
        return;
    }

    let src_path = &args[1];
    let dest_path = &args[2];

    let src_fd = open(src_path, open_flags::O_RDONLY);
    if src_fd < 0 {
        print("cp: cannot open source file: ");
        println(src_path);
        return;
    }

    let dest_fd = open(dest_path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if dest_fd < 0 {
        print("cp: cannot create destination file: ");
        println(dest_path);
        close(src_fd);
        return;
    }

    let mut buf = [0u8; 4096];
    loop {
        let n = read_fd(src_fd, &mut buf);
        if n <= 0 {
            break;
        }
        write_fd(dest_fd, &buf[..n as usize]);
    }

    close(src_fd);
    close(dest_fd);
}

fn cmd_mv(args: &[String]) {
    if args.len() < 3 {
        println("Usage: mv <src> <dest>");
        return;
    }

    if rename(&args[1], &args[2]) != 0 {
        println("mv: failed to rename/move");
    }
}

fn cmd_rm(args: &[String]) {
    if args.len() < 2 {
        println("Usage: rm <path>");
        return;
    }

    for path in &args[1..] {
        if unlink(path) != 0 {
            print("rm: failed to remove ");
            println(path);
        }
    }
}

fn cmd_find(args: &[String]) {
    let path = if args.len() < 2 {
        "."
    } else {
        &args[1]
    };

    let pattern = if args.len() >= 3 {
        Some(&args[2])
    } else {
        None
    };

    find_recursive(path, pattern);
}

fn find_recursive(path: &str, pattern: Option<&String>) {
    if let Some(reader) = read_dir(path) {
        for entry in reader {
            if entry.name == "." || entry.name == ".." {
                continue;
            }

            let full_path = if path == "/" {
                format!("/{}", entry.name)
            } else if path.ends_with('/') {
                format!("{}{}", path, entry.name)
            } else {
                format!("{}/{}", path, entry.name)
            };

            let matches = match pattern {
                Some(p) => entry.name.contains(p.as_str()),
                None => true,
            };

            if matches {
                println(&full_path);
            }

            if entry.is_dir {
                find_recursive(&full_path, pattern);
            }
        }
    }
}

fn cmd_grep(args: &[String]) {
    if args.len() < 3 {
        println("Usage: grep <pattern> <file>");
        return;
    }

    let pattern = &args[1];
    let file_path = &args[2];

    let fd = open(file_path, open_flags::O_RDONLY);
    if fd < 0 {
        print("grep: cannot open file: ");
        println(file_path);
        return;
    }

    let mut buf = [0u8; 4096];
    let mut current_line = String::new();

    loop {
        let n = read_fd(fd, &mut buf);
        if n <= 0 {
            break;
        }

        for &byte in &buf[..n as usize] {
            if byte == b'\n' {
                if current_line.contains(pattern.as_str()) {
                    println(&current_line);
                }
                current_line.clear();
            } else if byte != b'\r' {
                current_line.push(byte as char);
            }
        }
    }

    // Handle last line if it doesn't end with a newline
    if !current_line.is_empty() && current_line.contains(pattern.as_str()) {
        println(&current_line);
    }

    close(fd);
}

fn cmd_pkg(args: &[String]) {
    if args.len() < 3 || args[1] != "install" {
        println("Usage: pkg install <package1> [package2] ...");
        return;
    }

    for package in &args[2..] {
        install_package(package);
    }
}

fn install_package(package: &str) {
    print("pkg: downloading ");
    print(package);
    println("...");

    let url_path = format!("/target/aarch64-unknown-none/release/{}", package);
    let host = "10.0.2.2";
    let port = 8000;

    let addr_str = format!("{}:{}", host, port);
    let stream = match TcpStream::connect(&addr_str) {
        Ok(s) => s,
        Err(_) => {
            println("pkg: failed to connect to host server");
            return;
        }
    };

    let request = format!(
        "GET {} HTTP/1.0\r\n\
         Host: {}\r\n\
         User-Agent: paws/0.1.0\r\n\
         Connection: close\r\n\
         \r\n",
        url_path, host
    );

    if stream.write_all(request.as_bytes()).is_err() {
        println("pkg: failed to send request");
        return;
    }

    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }

    // Basic HTTP parsing (find \r\n\r\n)
    let mut headers_end = 0;
    for i in 0..response.len().saturating_sub(3) {
        if &response[i..i+4] == b"\r\n\r\n" {
            headers_end = i + 4;
            break;
        }
    }

    if headers_end == 0 {
        println("pkg: invalid response");
        return;
    }

    // Check status code 200
    let header_str = core::str::from_utf8(&response[..headers_end]).unwrap_or("");
    if !header_str.contains(" 200 ") {
        println("pkg: package not found or server error");
        return;
    }

    let body = &response[headers_end..];
    if body.is_empty() {
        println("pkg: empty package body");
        return;
    }

    let dest = format!("/bin/{}", package);
    let fd = open(&dest, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd < 0 {
        println("pkg: failed to open destination file");
        return;
    }

    if write_fd(fd, body) < 0 {
        println("pkg: failed to write package to disk");
    } else {
        print("pkg: installed ");
        print(package);
        print(" to ");
        println(&dest);
    }
    close(fd);
}

fn execute_external(args: &[String]) {
    let path = if args[0].starts_with('/') || args[0].starts_with("./") {
        args[0].clone()
    } else {
        format!("/bin/{}", args[0])
    };

    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    
    if let Some(res) = spawn(&path, Some(&arg_refs)) {
        let mut child_stdout_buf = [0u8; 1024];
        let mut user_input_buf = [0u8; 1];

        // Interactive loop for streaming output and catching Ctrl+C
        loop {
            // 1. Check for child output (non-blocking)
            let n = read_fd(res.stdout_fd as i32, &mut child_stdout_buf);
            if n > 0 {
                // Stream child output to our stdout
                write(fd::STDOUT, &child_stdout_buf[..n as usize]);
            }

            // 2. Check for user input (short timeout to keep loop responsive)
            let n_in = poll_input_event(10, &mut user_input_buf);
            if n_in > 0 {
                if user_input_buf[0] == 0x03 {
                    // Ctrl+C detected - send interrupt to child
                    println("^C");
                    kill(res.pid);
                } else {
                    // Forward other input to child stdin (if supported by kernel)
                    // Currently Akuma might not fully support writing to child's Stdin FD
                    // but we can try if there was a mechanism. For now we just catch Ctrl+C.
                }
            }

            // 3. Check if child has exited
            if let Some((_, exit_code)) = waitpid(res.pid) {
                // Final drain of stdout
                loop {
                    let n = read_fd(res.stdout_fd as i32, &mut child_stdout_buf);
                    if n <= 0 { break; }
                    write(fd::STDOUT, &child_stdout_buf[..n as usize]);
                }

                if exit_code != 0 && exit_code != 130 { // 130 is standard for SIGINT
                    print("[process exited with ");
                    print_dec(exit_code as usize);
                    println("]");
                }
                break;
            }

            // Small yield to prevent 100% CPU while waiting
            sleep_ms(5);
        }
    } else {
        print("paws: command not found: ");
        println(&args[0]);
    }
}

fn print_dec(n: usize) {
    let mut buf = [0u8; 20];
    let mut i = 19;
    let mut v = n;

    if v == 0 {
        print("0");
        return;
    }

    while v > 0 && i > 0 {
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        i -= 1;
    }

    if let Ok(s) = core::str::from_utf8(&buf[i + 1..]) {
        print(s);
    }
}
