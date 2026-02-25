#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::*;
use libakuma::net::TcpStream;

// Note: noshell is available in dependencies but we use a custom robust 
// parser here to avoid dependency on specific noshell versions/docs
// until we have more stable internet access for documentation.

#[no_mangle]
pub extern "C" fn main() {
    let args_iter = args();
    let args_vec: Vec<String> = args_iter.map(|s| String::from(s)).collect();
    
    // If -c is provided, execute the command and exit
    if args_vec.len() > 2 && args_vec[1] == "-c" {
        execute_line(&args_vec[2]);
        return;
    }

    run_shell();
}

fn run_shell() {
    println("\x1b[1;36mpaws v0.3.0\x1b[0m - OS Shell & Core Utilities");
    println("Type 'help' for available commands.");

    loop {
        print_prompt();
        let input = read_line();
        let trimmed = input.trim();
        
        if trimmed.is_empty() {
            continue;
        }

        execute_line(trimmed);
    }
}

fn print_prompt() {
    print("\x1b[1;32mpaws\x1b[0m ");
    print("\x1b[1;34m");
    print(getcwd());
    print("\x1b[0m");
    print(" # ");
}

fn execute_line(line: &str) {
    // 1. Handle command chains (;)
    for cmd in line.split(';') {
        let cmd = cmd.trim();
        if cmd.is_empty() { continue; }
        
        // 2. Handle pipelines (|)
        if let Some(pipe_pos) = cmd.find('|') {
            let left = cmd[..pipe_pos].trim();
            let right = cmd[pipe_pos + 1..].trim();
            execute_pipe(left, right);
        } 
        // 3. Handle redirection (>)
        else if let Some(redir_pos) = cmd.find('>') {
            let cmd_part = cmd[..redir_pos].trim();
            let mut file_part = cmd[redir_pos + 1..].trim();
            let mut append = false;
            
            if file_part.starts_with('>') {
                append = true;
                file_part = file_part[1..].trim();
            }
            
            execute_redirection(cmd_part, file_part, append);
        }
        // 4. Regular command
        else {
            execute_single_command(cmd);
        }
    }
}

fn execute_single_command(line: &str) {
    let args = parse_args(line);
    if args.is_empty() {
        return;
    }

    match args[0].as_str() {
        "exit" | "quit" => exit(0),
        "help" => cmd_help(),
        "pwd" => println(getcwd()),
        "cd" => cmd_cd(&args),
        "ls" => cmd_ls(&args),
        "cat" => cmd_cat(&args),
        "cp" => cmd_cp(&args),
        "mv" => cmd_mv(&args),
        "rm" => cmd_rm(&args),
        "mkdir" => cmd_mkdir(&args),
        "rmdir" => cmd_rmdir(&args),
        "touch" => cmd_touch(&args),
        "echo" => cmd_echo(&args),
        "uname" => cmd_uname(&args),
        "uptime" => cmd_uptime(&args),
        "sleep" => cmd_sleep(&args),
        "clear" => { clear_screen(); },
        "whoami" => println("akuma"),
        "pkg" => cmd_pkg(&args),
        "find" => cmd_find(&args),
        "grep" => cmd_grep(&args),
        "top" => execute_external(&args), // Use external top if available
        "dash" | "sh" => execute_external_reattach(&args),
        _ => execute_external(&args),
    }
}

fn execute_external_reattach(args: &[String]) {
    let path = find_bin(&args[0]);
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    
    // Debug: print arguments
    print("paws: executing (reattach) ");
    print(&path);
    for arg in arg_refs.iter().skip(1) {
        print(" ");
        print(arg);
    }
    println("");

    if let Some(res) = spawn(&path, Some(&arg_refs)) {
        // Delegate our I/O to the child
        reattach(res.pid);
        
        // Wait for child to exit
        loop {
            if let Some((_, exit_code)) = waitpid(res.pid) {
                print("paws: process ");
                print(&path);
                print(" exited with status ");
                print_dec(exit_code as usize);
                println("");
                break;
            }
            sleep_ms(10);
        }
    } else {
        print("paws: command not found: ");
        println(&args[0]);
    }
}

// ============================================================================
// Shell Pipeline & Redirection Logic
// ============================================================================

fn execute_redirection(cmd_line: &str, file_path: &str, append: bool) {
    // Implementation note: Ideally we'd have dup2() to redirect child process FDs.
    // For now, we capture builtin output and write it manually.
    let args = parse_args(cmd_line);
    if args.is_empty() { return; }

    let mut output = Vec::new();

    // Capture logic for supported builtins
    match args[0].as_str() {
        "echo" => {
            for (i, arg) in args.iter().enumerate().skip(1) {
                if i > 1 { output.extend_from_slice(b" "); }
                output.extend_from_slice(arg.as_bytes());
            }
            output.extend_from_slice(b"\n");
        }
        "ls" => {
            // Re-implementing ls to capture output
            let mut path = ".";
            for arg in args.iter().skip(1) {
                if !arg.starts_with('-') { path = arg; }
            }
            if let Some(reader) = read_dir(path) {
                for entry in reader {
                    output.extend_from_slice(entry.name.as_bytes());
                    if entry.is_dir { output.extend_from_slice(b"/"); }
                    output.extend_from_slice(b"  ");
                }
                output.extend_from_slice(b"\n");
            }
        }
        "pwd" => {
            output.extend_from_slice(getcwd().as_bytes());
            output.extend_from_slice(b"\n");
        }
        _ => {
            println("Redirection for this command is not yet implemented.");
            return;
        }
    }

    let flags = if append {
        open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_APPEND
    } else {
        open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC
    };

    let fd = open(file_path, flags);
    if fd >= 0 {
        write_fd(fd, &output);
        close(fd);
    } else {
        print("Failed to open file for redirection: ");
        println(file_path);
    }
}

fn execute_pipe(left_line: &str, right_line: &str) {
    let left_args = parse_args(left_line);
    if left_args.is_empty() { return; }

    // Execute left side and capture output
    let mut captured_output = Vec::new();

    // If left is builtin, capture manually
    match left_args[0].as_str() {
        "echo" | "ls" | "pwd" | "cat" => {
            // Simplified capture for builtins
            // In a real shell, we'd use a shared trait or buffer
            // For now, let's just use external for left side if it's simpler
            execute_external_and_capture(&left_args, &mut captured_output);
        }
        _ => {
            execute_external_and_capture(&left_args, &mut captured_output);
        }
    }

    if captured_output.is_empty() { return; }

    // Run right side with captured output as stdin
    let right_args = parse_args(right_line);
    if right_args.is_empty() { return; }

    match right_args[0].as_str() {
        "grep" => cmd_grep_with_stdin(&right_args, &captured_output),
        "cat" => { write(fd::STDOUT, &captured_output); }
        _ => {
            // Run external with stdin
            let path = find_bin(&right_args[0]);
            let arg_refs: Vec<&str> = right_args.iter().map(|s| s.as_str()).collect();
            if let Some(res) = spawn_with_stdin(&path, Some(&arg_refs), Some(&captured_output)) {
                stream_output(res.stdout_fd, res.pid);
            } else {
                print("paws: pipe target not found: ");
                println(&right_args[0]);
            }
        }
    }
}

// ============================================================================
// Command Implementations
// ============================================================================

fn cmd_help() {
    println("Embedded utilities:");
    println("  ls, cat, cp, mv, rm, mkdir, rmdir, touch, echo");
    println("  pwd, cd, uname, uptime, sleep, clear, whoami");
    println("  find, grep, pkg");
    println("\nShell features:");
    println("  Pipelines:  cmd1 | cmd2");
    println("  Redirect:   cmd > file, cmd >> file");
    println("  Chaining:   cmd1; cmd2");
}

fn cmd_ls(args: &[String]) {
    let mut show_all = false;
    let mut path = ".";

    for arg in args.iter().skip(1) {
        if arg == "-a" { show_all = true; }
        else if !arg.starts_with('-') { path = arg; }
    }

    if let Some(reader) = read_dir(path) {
        let mut entries: Vec<DirEntryInfo> = reader.collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));

        for entry in entries {
            if !show_all && entry.name.starts_with('.') && entry.name != "." && entry.name != ".." {
                continue;
            }
            if !show_all && entry.name.starts_with('.') { continue; }

            if entry.is_dir {
                print("\x1b[1;34m");
                print(&entry.name);
                print("\x1b[0m/");
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

fn cmd_cat(args: &[String]) {
    if args.len() < 2 { return; }
    for path in &args[1..] {
        let fd = open(path, open_flags::O_RDONLY);
        if fd < 0 { continue; }
        let mut buf = [0u8; 4096];
        loop {
            let n = read_fd(fd, &mut buf);
            if n <= 0 { break; }
            write(fd::STDOUT, &buf[..n as usize]);
        }
        close(fd);
    }
}

fn cmd_cp(args: &[String]) {
    if args.len() < 3 { return; }
    let src_fd = open(&args[1], open_flags::O_RDONLY);
    if src_fd < 0 { return; }
    let dest_fd = open(&args[2], open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if dest_fd < 0 { close(src_fd); return; }
    let mut buf = [0u8; 4096];
    loop {
        let n = read_fd(src_fd, &mut buf);
        if n <= 0 { break; }
        write_fd(dest_fd, &buf[..n as usize]);
    }
    close(src_fd);
    close(dest_fd);
}

fn cmd_mv(args: &[String]) {
    if args.len() < 3 { return; }
    let _ = rename(&args[1], &args[2]);
}

fn cmd_rm(args: &[String]) {
    for path in &args[1..] { let _ = unlink(path); }
}

fn cmd_mkdir(args: &[String]) {
    for path in &args[1..] { let _ = mkdir(path); }
}

fn cmd_rmdir(args: &[String]) {
    for path in &args[1..] { let _ = unlink(path); }
}

fn cmd_touch(args: &[String]) {
    for path in &args[1..] {
        let fd = open(path, open_flags::O_WRONLY | open_flags::O_CREAT);
        if fd >= 0 { close(fd); }
    }
}

fn cmd_echo(args: &[String]) {
    for (i, arg) in args.iter().enumerate().skip(1) {
        if i > 1 { print(" "); }
        print(arg);
    }
    println("");
}

fn cmd_cd(args: &[String]) {
    let target = if args.len() < 2 { "/" } else { &args[1] };
    let _ = chdir(target);
}

fn cmd_uname(args: &[String]) {
    if args.len() > 1 && args[1] == "-a" {
        println("Akuma 0.1.0 Akuma-OS aarch64");
    } else {
        println("Akuma");
    }
}

fn cmd_uptime(_args: &[String]) {
    let us = uptime();
    let sec = us / 1_000_000;
    print("up ");
    print_dec((sec / 3600) as usize);
    print(":");
    print_dec(((sec % 3600) / 60) as usize);
    print(":");
    print_dec((sec % 60) as usize);
    println("");
}

fn cmd_sleep(args: &[String]) {
    if args.len() < 2 { return; }
    let sec: u64 = args[1].parse().unwrap_or(0);
    sleep(sec);
}

fn cmd_pkg(args: &[String]) {
    if args.len() < 2 {
        println("Usage: pkg install <package>");
        return;
    }
    // The first argument to the process is the command itself, which is "pkg"
    execute_external_with_status(args);
}

fn cmd_find(args: &[String]) {
    let path = if args.len() < 2 { "." } else { &args[1] };
    let pattern = if args.len() >= 3 { Some(&args[2]) } else { None };
    find_recursive(path, pattern);
}

fn find_recursive(path: &str, pattern: Option<&String>) {
    if let Some(reader) = read_dir(path) {
        for entry in reader {
            if entry.name == "." || entry.name == ".." { continue; }
            let full_path = format!("{}/{}", if path == "/" { "" } else { path }, entry.name);
            let matches = pattern.map_or(true, |p| entry.name.contains(p.as_str()));
            if matches { println(&full_path); }
            if entry.is_dir { find_recursive(&full_path, pattern); }
        }
    }
}

fn cmd_grep(args: &[String]) {
    if args.len() < 2 { return; }
    if args.len() < 3 { return; }
    let pattern = &args[1];
    let fd = open(&args[2], open_flags::O_RDONLY);
    if fd < 0 { return; }
    let mut buf = [0u8; 4096];
    let mut line = String::new();
    loop {
        let n = read_fd(fd, &mut buf);
        if n <= 0 { break; }
        for &b in &buf[..n as usize] {
            if b == b'\n' {
                if line.contains(pattern.as_str()) { println(&line); }
                line.clear();
            } else if b != b'\r' { line.push(b as char); }
        }
    }
    close(fd);
}

fn cmd_grep_with_stdin(args: &[String], input: &[u8]) {
    if args.len() < 2 { return; }
    let pattern = &args[1];
    let mut line = String::new();
    for &b in input {
        if b == b'\n' {
            if line.contains(pattern.as_str()) { println(&line); }
            line.clear();
        } else if b != b'\r' { line.push(b as char); }
    }
}

// ============================================================================
// Helpers & System Integration
// ============================================================================

fn find_bin(name: &str) -> String {
    if name.starts_with('/') || name.starts_with("./") {
        return String::from(name);
    }

    let paths = ["/usr/bin", "/bin"];
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

fn execute_external(args: &[String]) {
    execute_external_with_status(args);
}

fn execute_external_with_status(args: &[String]) -> i32 {
    let path = find_bin(&args[0]);
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    
    // Debug: print arguments
    print("paws: executing ");
    print(&path);
    for arg in arg_refs.iter().skip(1) {
        print(" ");
        print(arg);
    }
    println("");

    if let Some(res) = spawn(&path, Some(&arg_refs)) {
        let status = stream_output(res.stdout_fd, res.pid);
        print("paws: process ");
        print(&path);
        print(" exited with status ");
        print_dec(status as usize);
        println("");
        return status;
    } else {
        print("paws: command not found: ");
        println(&args[0]);
        return -1;
    }
}

fn execute_external_and_capture(args: &[String], output: &mut Vec<u8>) {
    let path = find_bin(&args[0]);
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    if let Some(res) = spawn(&path, Some(&arg_refs)) {
        let mut buf = [0u8; 4096];
        loop {
            let n = read_fd(res.stdout_fd as i32, &mut buf);
            if n > 0 { output.extend_from_slice(&buf[..n as usize]); }
            if let Some(_) = waitpid(res.pid) {
                while read_fd(res.stdout_fd as i32, &mut buf) > 0 {}
                break;
            }
            sleep_ms(1);
        }
    }
}

fn stream_output(stdout_fd: u32, pid: u32) -> i32 {
    let mut buf = [0u8; 1024];
    let mut in_buf = [0u8; 1];
    loop {
        let n = read_fd(stdout_fd as i32, &mut buf);
        if n > 0 { write(fd::STDOUT, &buf[..n as usize]); }
        
        if poll_input_event(10, &mut in_buf) > 0 && in_buf[0] == 0x03 {
            println("^C");
            kill(pid);
        }
        
        if let Some((_, exit_code)) = waitpid(pid) {
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

fn read_line() -> String {
    let mut line = String::new();
    let mut buf = [0u8; 1];
    loop {
        let n = poll_input_event(core::u64::MAX, &mut buf);
        if n <= 0 {
            if n == 0 && line.is_empty() { println("exit"); exit(0); }
            break;
        }
        let c = buf[0];
        if c == b'\n' || c == b'\r' { println(""); break; }
        else if c == 8 || c == 127 {
            if !line.is_empty() { line.pop(); print("\x08 \x08"); }
        } else if c == 4 { // Ctrl+D
            if line.is_empty() { println("exit"); exit(0); }
            break;
        } else if c >= 32 {
            line.push(c as char);
            print(core::str::from_utf8(&buf).unwrap_or(""));
        }
    }
    line
}

fn parse_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in input.chars() {
        if c == '"' { in_quotes = !in_quotes; }
        else if c.is_whitespace() && !in_quotes {
            if !current.is_empty() { args.push(current.clone()); current.clear(); }
        } else { current.push(c); }
    }
    if !current.is_empty() { args.push(current); }
    args
}
