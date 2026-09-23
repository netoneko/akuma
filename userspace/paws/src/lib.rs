//! paws — Akuma's own minimal shell, as a library.
//!
//! Two hosts drive the same command set:
//!
//! - **the `paws` binary** (`main.rs`), [`Mode::Standalone`]: a process of its
//!   own, output straight to stdout, external commands spawned and streamed.
//! - **`sshd`'s `builtin-paws` feature**, [`Mode::Embedded`]: the shell runs
//!   *inside* the sshd process, as the last fallback when neither the
//!   configured shell nor `/bin/paws` can be spawned — i.e. when exec itself is
//!   what broke (a USB root whose reads are failing, a `/bin` that lost its
//!   links). In that state the one thing worth guaranteeing is a `reboot` that
//!   needs no further exec, and that is what this mode is for.
//!
//! Embedded mode therefore runs **builtins only**, and never blocks: sshd on
//! amd64 is one cooperative executor serving every session, so a `sleep` or a
//! wait on a child here would freeze all of them. Output goes to an [`Out`]
//! sink the host flushes after each command, and the two commands that must
//! not run inside the host's process — `exit` and `reboot` — come back as a
//! [`Flow`] for the host to act on instead of being performed here.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::*;

/// Where a command's output goes.
pub trait Out {
    fn write(&mut self, bytes: &[u8]);
    /// Whether ANSI colour is wanted. False for anything captured into a file
    /// or a pipe, so `ls > f` doesn't write escape codes into `f`.
    fn color(&self) -> bool {
        true
    }
    fn str(&mut self, s: &str) {
        self.write(s.as_bytes());
    }
    fn line(&mut self, s: &str) {
        self.write(s.as_bytes());
        self.write(b"\n");
    }
    fn dec(&mut self, v: usize) {
        self.str(&format!("{v}"));
    }
}

/// The binary's stdout, unbuffered — output lands in order with anything an
/// external child writes through the same fd.
pub struct Stdout;

impl Out for Stdout {
    fn write(&mut self, bytes: &[u8]) {
        libakuma::write(fd::STDOUT, bytes);
    }
}

/// A capped in-memory sink: a pipe's left side, a redirection's source, or an
/// embedded session's per-command output.
///
/// Capped because embedded mode buffers a whole command's output before the
/// host can send any of it, and `cat` of a large file must not be able to run
/// sshd out of memory. Bytes past the cap are dropped and `truncated` is set.
pub struct Buf {
    pub bytes: Vec<u8>,
    pub truncated: bool,
    color: bool,
    cap: usize,
}

impl Buf {
    pub const DEFAULT_CAP: usize = 1 << 20;

    pub fn new(color: bool) -> Self {
        Self::with_cap(color, Self::DEFAULT_CAP)
    }

    pub fn with_cap(color: bool, cap: usize) -> Self {
        Self { bytes: Vec::new(), truncated: false, color, cap }
    }

    /// Hand the bytes over and start empty again, keeping the settings.
    pub fn take(&mut self) -> Vec<u8> {
        self.truncated = false;
        core::mem::take(&mut self.bytes)
    }
}

impl Out for Buf {
    fn write(&mut self, bytes: &[u8]) {
        let room = self.cap.saturating_sub(self.bytes.len());
        if bytes.len() > room {
            self.truncated = true;
        }
        self.bytes.extend_from_slice(&bytes[..bytes.len().min(room)]);
    }
    fn color(&self) -> bool {
        self.color
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Its own process: may spawn, wait, sleep, exit and reboot directly.
    Standalone,
    /// Inside sshd: builtins only, nothing that blocks, and `exit`/`reboot`
    /// are returned to the host rather than performed.
    Embedded,
}

/// What the host should do after a line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flow {
    /// Keep going; the status of the last command in the line.
    Continue(i32),
    /// `exit [code]`.
    Exit(i32),
    /// `reboot`, in [`Mode::Embedded`] only — the host sends whatever output is
    /// pending first, then calls `libakuma::reboot()`. Standalone reboots
    /// in place.
    Reboot,
}

pub fn banner(out: &mut dyn Out) {
    out.line("\x1b[1;36mpaws v0.3.0\x1b[0m - OS Shell & Core Utilities");
    out.line("Type 'help' for available commands.");
}

pub fn prompt(out: &mut dyn Out) {
    out.str("\x1b[1;32mpaws\x1b[0m \x1b[1;34m");
    out.str(getcwd());
    out.str("\x1b[0m # ");
}

/// Run one input line: `;` chains, one `|` or one `>`/`>>` per command.
pub fn execute_line(line: &str, mode: Mode, out: &mut dyn Out) -> Flow {
    let mut status = 0;
    for cmd in line.split(';') {
        let cmd = cmd.trim();
        if cmd.is_empty() {
            continue;
        }

        let flow = if let Some(pipe_pos) = cmd.find('|') {
            let left = cmd[..pipe_pos].trim();
            let right = cmd[pipe_pos + 1..].trim();
            execute_pipe(left, right, mode, out)
        } else if let Some(redir_pos) = cmd.find('>') {
            let cmd_part = cmd[..redir_pos].trim();
            let mut file_part = cmd[redir_pos + 1..].trim();
            let mut append = false;
            if let Some(rest) = file_part.strip_prefix('>') {
                append = true;
                file_part = rest.trim();
            }
            execute_redirection(cmd_part, file_part, append, mode, out)
        } else {
            execute_single_command(&parse_args(cmd), mode, out)
        };

        match flow {
            Flow::Continue(s) => status = s,
            other => return other,
        }
    }
    Flow::Continue(status)
}

fn execute_single_command(args: &[String], mode: Mode, out: &mut dyn Out) -> Flow {
    if args.is_empty() {
        return Flow::Continue(0);
    }
    if let Some(flow) = run_builtin(args, mode, out) {
        return flow;
    }
    if mode == Mode::Embedded {
        return not_in_builtin_shell(&args[0], out);
    }
    match args[0].as_str() {
        "dash" | "sh" => Flow::Continue(execute_external_reattach(args, out)),
        _ => Flow::Continue(execute_external(args, out)),
    }
}

fn not_in_builtin_shell(name: &str, out: &mut dyn Out) -> Flow {
    out.str("paws: ");
    out.str(name);
    out.line(": not a builtin (this is sshd's built-in shell: builtins only, no exec)");
    Flow::Continue(127)
}

/// A builtin, if `args[0]` names one. `None` means "not a builtin".
fn run_builtin(args: &[String], mode: Mode, out: &mut dyn Out) -> Option<Flow> {
    let ok = Flow::Continue(0);
    Some(match args[0].as_str() {
        "exit" | "quit" => {
            let code = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            if mode == Mode::Standalone {
                exit(code);
            }
            Flow::Exit(code)
        }
        "help" => { cmd_help(mode, out); ok }
        "pwd" => { out.line(getcwd()); ok }
        "cd" => { cmd_cd(args); ok }
        "ls" => { cmd_ls(args, out); ok }
        "cat" => { cmd_cat(args, out); ok }
        "cp" => { cmd_cp(args); ok }
        "mv" => { cmd_mv(args); ok }
        "rm" => { cmd_rm(args); ok }
        "mkdir" => { cmd_mkdir(args); ok }
        "rmdir" => { cmd_rmdir(args); ok }
        "touch" => { cmd_touch(args); ok }
        "echo" => { cmd_echo(args, out); ok }
        "uname" => { cmd_uname(args, out); ok }
        "uptime" => { cmd_uptime(out); ok }
        "sleep" => {
            if mode == Mode::Embedded {
                // Would park sshd's whole executor, not just this session.
                out.line("paws: sleep is not available in sshd's built-in shell");
                return Some(Flow::Continue(1));
            }
            cmd_sleep(args);
            ok
        }
        "clear" => {
            if mode == Mode::Embedded {
                // `clear_screen()` clears the kernel console, not the ssh
                // client's terminal.
                out.str("\x1b[2J\x1b[H");
            } else {
                clear_screen();
            }
            ok
        }
        "whoami" => { out.line("akuma"); ok }
        "free" => { cmd_free(out); ok }
        "find" => { cmd_find(args, out); ok }
        "grep" => { cmd_grep(args, out); ok }
        "reboot" => {
            if mode == Mode::Embedded {
                return Some(Flow::Reboot);
            }
            cmd_reboot(out);
            Flow::Continue(1)
        }
        _ => return None,
    })
}

// ============================================================================
// Shell Pipeline & Redirection Logic
// ============================================================================

fn execute_redirection(cmd_line: &str, file_path: &str, append: bool, mode: Mode, out: &mut dyn Out) -> Flow {
    let args = parse_args(cmd_line);
    if args.is_empty() {
        return Flow::Continue(0);
    }

    // Implementation note: there is no dup2() onto a child's fds here, so only
    // a builtin's output can be redirected — captured, then written.
    let mut captured = Buf::new(false);
    let flow = match run_builtin(&args, mode, &mut captured) {
        Some(flow @ Flow::Continue(_)) => flow,
        Some(other) => return other,
        None => {
            out.line("Redirection for this command is not yet implemented.");
            return Flow::Continue(1);
        }
    };

    let flags = if append {
        open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_APPEND
    } else {
        open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC
    };

    let fd = open(file_path, flags);
    if fd >= 0 {
        write_fd(fd, &captured.bytes);
        close(fd);
        flow
    } else {
        out.str("Failed to open file for redirection: ");
        out.line(file_path);
        Flow::Continue(1)
    }
}

fn execute_pipe(left_line: &str, right_line: &str, mode: Mode, out: &mut dyn Out) -> Flow {
    let left_args = parse_args(left_line);
    if left_args.is_empty() {
        return Flow::Continue(0);
    }

    // Left side. Standalone always spawns it, as paws always has — so `ls |
    // grep x` gets busybox's one-name-per-line `ls`, not the builtin's single
    // line. Embedded cannot spawn, so there only a builtin can feed a pipe.
    let mut captured = Buf::new(false);
    if mode == Mode::Standalone {
        execute_external_and_capture(&left_args, &mut captured.bytes);
    } else {
        match run_builtin(&left_args, mode, &mut captured) {
            Some(Flow::Continue(_)) => {}
            Some(other) => return other,
            None => return not_in_builtin_shell(&left_args[0], out),
        }
    }
    let captured = captured.bytes;

    if captured.is_empty() {
        return Flow::Continue(0);
    }

    let right_args = parse_args(right_line);
    if right_args.is_empty() {
        return Flow::Continue(0);
    }

    match right_args[0].as_str() {
        "grep" => cmd_grep_with_stdin(&right_args, &captured, out),
        "cat" => out.write(&captured),
        _ if mode == Mode::Embedded => return not_in_builtin_shell(&right_args[0], out),
        _ => {
            let path = find_bin(&right_args[0]);
            // skip(1): argv[0] is added by `spawn` — see `execute_external`.
            let arg_refs: Vec<&str> = right_args.iter().skip(1).map(|s| s.as_str()).collect();
            if let Some(res) = spawn_with_stdin(&path, Some(&arg_refs), Some(&captured)) {
                return Flow::Continue(stream_output(res.stdout_fd, res.pid, out));
            }
            out.str("paws: pipe target not found: ");
            out.line(&right_args[0]);
            return Flow::Continue(127);
        }
    }
    Flow::Continue(0)
}

// ============================================================================
// Command Implementations
// ============================================================================

fn cmd_help(mode: Mode, out: &mut dyn Out) {
    out.line("Embedded utilities:");
    out.line("  ls, cat, cp, mv, rm, mkdir, rmdir, touch, echo");
    out.line("  pwd, cd, uname, uptime, sleep, clear, whoami");
    out.line("  free, find, grep, reboot, exit");
    out.line("\nShell features:");
    out.line("  Pipelines:  cmd1 | cmd2");
    out.line("  Redirect:   cmd > file, cmd >> file");
    out.line("  Chaining:   cmd1; cmd2");
    if mode == Mode::Embedded {
        out.line("\nRunning inside sshd: builtins only, nothing is exec'd,");
        out.line("and `sleep` is disabled. `reboot` calls reboot(2) directly.");
    }
}

fn cmd_ls(args: &[String], out: &mut dyn Out) {
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
            if !show_all && entry.name.starts_with('.') { continue; }

            if entry.is_dir {
                if out.color() { out.str("\x1b[1;34m"); }
                out.str(&entry.name);
                if out.color() { out.str("\x1b[0m"); }
                out.str("/");
            } else {
                out.str(&entry.name);
            }
            out.str("  ");
        }
        out.line("");
    } else {
        out.str("ls: cannot access ");
        out.line(path);
    }
}

fn cmd_cat(args: &[String], out: &mut dyn Out) {
    if args.len() < 2 { return; }
    for path in &args[1..] {
        let fd = open(path, open_flags::O_RDONLY);
        if fd < 0 { continue; }
        let mut buf = [0u8; 4096];
        loop {
            let n = read_fd(fd, &mut buf);
            if n <= 0 { break; }
            out.write(&buf[..n as usize]);
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

fn cmd_echo(args: &[String], out: &mut dyn Out) {
    for (i, arg) in args.iter().enumerate().skip(1) {
        if i > 1 { out.str(" "); }
        out.str(arg);
    }
    out.line("");
}

fn cmd_cd(args: &[String]) {
    let target = if args.len() < 2 { "/" } else { &args[1] };
    let _ = chdir(target);
}

fn cmd_uname(args: &[String], out: &mut dyn Out) {
    // The machine name is the one this copy of paws was compiled for, not a
    // constant: paws builds for both `aarch64-unknown-linux-musl` and
    // `x86_64-unknown-none` (the amd64 target), and the hardcoded "aarch64"
    // here reported the wrong architecture over every ssh session into the
    // x86 kernel — the first thing anyone types after logging in.
    #[cfg(target_arch = "x86_64")]
    const MACHINE: &str = "x86_64";
    #[cfg(not(target_arch = "x86_64"))]
    const MACHINE: &str = "aarch64";

    if args.len() > 1 && args[1] == "-a" {
        out.line(&format!("Akuma 0.1.0 Akuma-OS {MACHINE}"));
    } else {
        out.line("Akuma");
    }
}

fn cmd_uptime(out: &mut dyn Out) {
    let sec = uptime() / 1_000_000;
    out.line(&format!("up {}:{}:{}", sec / 3600, (sec % 3600) / 60, sec % 60));
}

fn cmd_sleep(args: &[String]) {
    if args.len() < 2 { return; }
    let sec: u64 = args[1].parse().unwrap_or(0);
    sleep(sec);
}

/// `reboot` — calls `reboot(2)` directly, with no external binary in the
/// path. This is what makes it safe to rely on as the *last* recovery
/// command: if `/bin/busybox` (or whatever `reboot`/`halt` normally shells
/// out to) is missing or corrupted, this still works, because there is
/// nothing here to exec — `libakuma::reboot()` is a raw syscall. In
/// [`Mode::Embedded`] the host (sshd) makes the same call itself, after
/// flushing, via [`Flow::Reboot`].
fn cmd_reboot(out: &mut dyn Out) {
    out.line("paws: rebooting...");
    let rc = reboot();
    // Only reachable on failure — a successful reboot(2) never returns.
    out.line(&format!("paws: reboot failed, errno {}", -rc));
}

/// `free` — total/used/free RAM straight from `sysinfo(2)` (syscall 179).
///
/// There is no /proc/meminfo on this kernel, so this is the only way a userspace
/// process can see the PMM's page counts. The kernel fills `mem_unit = 1`, so
/// `totalram`/`freeram` are already bytes. Layout per src/syscall/proc.rs:
/// offset 0 = uptime, 32 = totalram, 40 = freeram.
fn cmd_free(out: &mut dyn Out) {
    const SYS_SYSINFO: u64 = 179;
    let mut buf = [0u8; 112];
    let rc = syscall(SYS_SYSINFO, buf.as_mut_ptr() as u64, 0, 0, 0, 0, 0);
    if rc != 0 {
        out.line("free: sysinfo failed");
        return;
    }
    let rd = |off: usize| -> u64 {
        let mut v: u64 = 0;
        let mut i = 8;
        while i > 0 {
            i -= 1;
            v = (v << 8) | u64::from(buf[off + i]);
        }
        v
    };
    let (uptime, total, free) = (rd(0), rd(32), rd(40));
    out.line(&format!(
        "         total       used       free\nMem:  {:>8} K {:>8} K {:>8} K\nuptime: {}s",
        total / 1024,
        (total - free) / 1024,
        free / 1024,
        uptime
    ));
}

fn cmd_find(args: &[String], out: &mut dyn Out) {
    let path = if args.len() < 2 { "." } else { &args[1] };
    let pattern = if args.len() >= 3 { Some(&args[2]) } else { None };
    find_recursive(path, pattern, out);
}

fn find_recursive(path: &str, pattern: Option<&String>, out: &mut dyn Out) {
    if let Some(reader) = read_dir(path) {
        for entry in reader {
            if entry.name == "." || entry.name == ".." { continue; }
            let full_path = format!("{}/{}", if path == "/" { "" } else { path }, entry.name);
            let matches = pattern.map_or(true, |p| entry.name.contains(p.as_str()));
            if matches { out.line(&full_path); }
            if entry.is_dir { find_recursive(&full_path, pattern, out); }
        }
    }
}

fn cmd_grep(args: &[String], out: &mut dyn Out) {
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
                if line.contains(pattern.as_str()) { out.line(&line); }
                line.clear();
            } else if b != b'\r' { line.push(b as char); }
        }
    }
    close(fd);
}

fn cmd_grep_with_stdin(args: &[String], input: &[u8], out: &mut dyn Out) {
    if args.len() < 2 { return; }
    let pattern = &args[1];
    let mut line = String::new();
    for &b in input {
        if b == b'\n' {
            if line.contains(pattern.as_str()) { out.line(&line); }
            line.clear();
        } else if b != b'\r' { line.push(b as char); }
    }
}

// ============================================================================
// External commands (Mode::Standalone only)
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

/// `skip(1)` in the spawners below: `args[0]` is the command NAME, and the
/// `spawn` syscall builds argv itself as `[path, ...args]` — passing the name
/// through as an argument too puts it at argv[1], where it reads as the first
/// positional operand. busybox hides this (a leading applet name is exactly how
/// the multicall binary is meant to be invoked, so it just re-dispatches),
/// which is why it went unnoticed; `tcc` took the duplicate as an input file
/// and died with `file 'tcc' not found` before compiling anything.
fn external_args(args: &[String]) -> Vec<&str> {
    args.iter().skip(1).map(|s| s.as_str()).collect()
}

fn print_exec(tag: &str, path: &str, arg_refs: &[&str], out: &mut dyn Out) {
    out.str(tag);
    out.str(path);
    for arg in arg_refs {
        out.str(" ");
        out.str(arg);
    }
    out.line("");
}

fn execute_external_reattach(args: &[String], out: &mut dyn Out) -> i32 {
    let path = find_bin(&args[0]);
    let arg_refs = external_args(args);
    print_exec("paws: executing (reattach) ", &path, &arg_refs, out);

    if let Some(res) = spawn(&path, Some(&arg_refs)) {
        // Delegate our I/O to the child. Freshly spawned — nobody else can
        // already hold it, so there's nothing to force past.
        reattach(res.pid, false);

        loop {
            if let Some((_, exit_code)) = waitpid(res.pid) {
                out.line(&format!("paws: process {path} exited with status {exit_code}"));
                return exit_code;
            }
            sleep_ms(10);
        }
    }
    out.str("paws: command not found: ");
    out.line(&args[0]);
    127
}

fn execute_external(args: &[String], out: &mut dyn Out) -> i32 {
    let path = find_bin(&args[0]);
    let arg_refs = external_args(args);
    print_exec("paws: executing ", &path, &arg_refs, out);

    if let Some(res) = spawn(&path, Some(&arg_refs)) {
        let status = stream_output(res.stdout_fd, res.pid, out);
        out.line(&format!("paws: process {path} exited with status {status}"));
        return status;
    }
    out.str("paws: command not found: ");
    out.line(&args[0]);
    127
}

fn execute_external_and_capture(args: &[String], output: &mut Vec<u8>) {
    let path = find_bin(&args[0]);
    let arg_refs = external_args(args);
    if let Some(res) = spawn(&path, Some(&arg_refs)) {
        let mut buf = [0u8; 4096];
        loop {
            let n = read_fd(res.stdout_fd as i32, &mut buf);
            if n > 0 { output.extend_from_slice(&buf[..n as usize]); }
            if waitpid(res.pid).is_some() {
                while read_fd(res.stdout_fd as i32, &mut buf) > 0 {}
                break;
            }
            sleep_ms(1);
        }
    }
}

fn stream_output(stdout_fd: u32, pid: u32, out: &mut dyn Out) -> i32 {
    // `stdout_fd` (the spawned child's stdout) blocks by default, same as any
    // other pipe fd on this kernel. Without this, `read_fd` below parks
    // waiting for the child's next byte of output — and a child that's gone
    // quiet (a server between requests, a long-running build between lines)
    // never produces one, so the loop never gets back around to
    // poll_input_event and Ctrl-C is never seen. sshd's own bridge_process
    // (userspace/sshd/src/protocol.rs) hit and documented this exact
    // deadlock for the same reason; this mirrors its fix.
    set_nonblocking(stdout_fd as i32, true);
    let mut buf = [0u8; 1024];
    let mut in_buf = [0u8; 1];
    loop {
        let n = read_fd(stdout_fd as i32, &mut buf);
        if n > 0 { out.write(&buf[..n as usize]); }

        if poll_input_event(10, &mut in_buf) > 0 && in_buf[0] == 0x03 {
            out.line("^C");
            kill_signal(pid, SIGINT);
        }

        if let Some((_, exit_code)) = waitpid(pid) {
            loop {
                let n = read_fd(stdout_fd as i32, &mut buf);
                if n <= 0 { break; }
                out.write(&buf[..n as usize]);
            }
            return exit_code;
        }
        sleep_ms(5);
    }
}

// ============================================================================
// Line editing
// ============================================================================

/// One keystroke's result.
#[derive(PartialEq, Eq, Debug)]
pub enum Edit {
    /// Still typing.
    Pending,
    /// Enter (or Ctrl-D on a non-empty line): run this.
    Line(String),
    /// Ctrl-D on an empty line.
    Eof,
    /// Ctrl-C: the line was discarded; print a fresh prompt.
    Interrupt,
}

/// Byte-at-a-time line editor, echoing to `out` — shared by the binary's
/// blocking read loop and sshd's channel-driven one, so both edit alike.
#[derive(Default)]
pub struct LineEditor {
    line: String,
}

impl LineEditor {
    pub const fn new() -> Self {
        Self { line: String::new() }
    }

    pub fn feed(&mut self, c: u8, out: &mut dyn Out) -> Edit {
        match c {
            b'\n' | b'\r' => {
                out.line("");
                Edit::Line(core::mem::take(&mut self.line))
            }
            8 | 127 => {
                if self.line.pop().is_some() {
                    out.str("\x08 \x08");
                }
                Edit::Pending
            }
            3 => {
                self.line.clear();
                out.line("^C");
                Edit::Interrupt
            }
            4 => {
                if self.line.is_empty() {
                    out.line("exit");
                    Edit::Eof
                } else {
                    Edit::Line(core::mem::take(&mut self.line))
                }
            }
            c if c >= 32 => {
                self.line.push(c as char);
                out.write(&[c]);
                Edit::Pending
            }
            _ => Edit::Pending,
        }
    }
}

pub fn parse_args(input: &str) -> Vec<String> {
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
