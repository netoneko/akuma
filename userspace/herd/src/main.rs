//! Herd - Userspace Process Supervisor
//!
//! A process supervisor that manages background services.
//! Named "herd" because herding cats is an apt metaphor for managing processes.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::{
    print, exit, open, read_fd, write_fd, close, fstat, lseek,
    open_flags, seek_mode, spawn, kill, waitpid, read_dir, uptime,
    sleep_ms, mkdir_p, SpawnResult,
};

// ============================================================================
// Constants
// ============================================================================

/// Config reload interval in milliseconds (20 seconds)
const CONFIG_RELOAD_INTERVAL_MS: u64 = 20_000;

/// Supervisor poll interval in milliseconds
const POLL_INTERVAL_MS: u64 = 100;

/// Maximum log file size before rotation (32KB)
const MAX_LOG_SIZE: usize = 32 * 1024;

/// Default restart delay in milliseconds
const DEFAULT_RESTART_DELAY_MS: u64 = 1000;

/// Default max retries (0 = infinite)
const DEFAULT_MAX_RETRIES: u32 = 0;

/// Herd directories
const HERD_ENABLED_DIR: &str = "/etc/herd/enabled";
const HERD_AVAILABLE_DIR: &str = "/etc/herd/available";
const HERD_LOG_DIR: &str = "/var/log/herd";

// ============================================================================
// Directory Setup
// ============================================================================

/// Ensure all required directories exist
fn ensure_directories() {
    // Create /etc/herd/enabled
    if mkdir_p(HERD_ENABLED_DIR) {
        // Only print if we are sure it didn't exist or we don't care to be too verbose
    } else {
        print("[herd] Warning: Failed to create ");
        print(HERD_ENABLED_DIR);
        print("\n");
    }
    
    // Create /etc/herd/available
    if !mkdir_p(HERD_AVAILABLE_DIR) {
        print("[herd] Warning: Failed to create ");
        print(HERD_AVAILABLE_DIR);
        print("\n");
    }
    
    // Create /var/log/herd
    if !mkdir_p(HERD_LOG_DIR) {
        print("[herd] Warning: Failed to create ");
        print(HERD_LOG_DIR);
        print("\n");
    }
}

// ============================================================================
// Service State
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceState {
    Stopped,
    Running,
    Failed,
    PendingRestart,
}

// ============================================================================
// Service Configuration
// ============================================================================

#[derive(Clone)]
struct ServiceConfig {
    command: String,
    args: Vec<String>,
    restart_delay_ms: u64,
    max_retries: u32,
    boxed: bool,
    box_root: String,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            restart_delay_ms: DEFAULT_RESTART_DELAY_MS,
            max_retries: DEFAULT_MAX_RETRIES,
            boxed: false,
            box_root: String::from("/"),
        }
    }
}

// ============================================================================
// Supervised Process
// ============================================================================

struct SupervisedProcess {
    name: String,
    config: ServiceConfig,
    pid: Option<u32>,
    stdout_fd: Option<u32>,
    state: ServiceState,
    restart_count: u32,
    last_exit_code: Option<i32>,
    restart_at_ms: Option<u64>,
    log_size: usize,
}

impl SupervisedProcess {
    fn new(name: String, config: ServiceConfig) -> Self {
        Self {
            name,
            config,
            pid: None,
            stdout_fd: None,
            state: ServiceState::Stopped,
            restart_count: 0,
            last_exit_code: None,
            restart_at_ms: None,
            log_size: 0,
        }
    }
}

// ============================================================================
// Herd State
// ============================================================================

struct HerdState {
    services: BTreeMap<String, SupervisedProcess>,
    last_config_reload_ms: u64,
}

impl HerdState {
    fn new() -> Self {
        Self {
            services: BTreeMap::new(),
            last_config_reload_ms: 0,
        }
    }
}

// ============================================================================
// Entry Point
// ============================================================================

#[no_mangle]
pub extern "C" fn _start() -> ! {
    main();
    exit(0);
}

fn main() {
    // Ensure required directories exist
    ensure_directories();

    // Check for command-line arguments
    let argc = libakuma::argc();
    
    if argc > 1 {
        // Command mode - handle subcommand
        let subcommand = libakuma::arg(1).unwrap_or("");
        let service_name = libakuma::arg(2);
        
        match subcommand {
            "daemon" | "run" | "foreground" | "fg" => {
                // Run as daemon in foreground (fall through to supervisor loop)
            }
            "status" => {
                cmd_status();
                return;
            }
            "add" => {
                if let Some(name) = service_name {
                    cmd_add(name);
                } else {
                    print("Usage: herd add <service>\n");
                }
                return;
            }
            "config" => {
                if let Some(name) = service_name {
                    cmd_config(name);
                } else {
                    print("Usage: herd config <service>\n");
                }
                return;
            }
            "enable" => {
                if let Some(name) = service_name {
                    cmd_enable(name);
                } else {
                    print("Usage: herd enable <service>\n");
                }
                return;
            }
            "disable" => {
                if let Some(name) = service_name {
                    cmd_disable(name);
                } else {
                    print("Usage: herd disable <service>\n");
                }
                return;
            }
            "log" => {
                if let Some(name) = service_name {
                    cmd_log(name);
                } else {
                    print("Usage: herd log <service>\n");
                }
                return;
            }
            "help" | "--help" | "-h" => {
                print_usage();
                return;
            }
            _ => {
                print("Unknown command: ");
                print(subcommand);
                print("\n");
                print_usage();
                return;
            }
        }
    }

    // Daemon mode - run supervisor loop
    print("[herd] Userspace supervisor starting...\n");

    let mut state = HerdState::new();

    // Initial config load
    reload_config(&mut state);

    // Start enabled services
    start_stopped_services(&mut state);

    // Main supervisor loop
    loop {
        let now_ms = uptime() / 1000; // uptime() returns microseconds

        // 1. Poll stdout from running services
        poll_all_stdout(&mut state);

        // 2. Check for exited processes
        check_process_exits(&mut state, now_ms);

        // 3. Handle pending restarts
        process_pending_restarts(&mut state, now_ms);

        // 4. Reload config every 20 seconds
        if now_ms.saturating_sub(state.last_config_reload_ms) >= CONFIG_RELOAD_INTERVAL_MS {
            print("[herd] Reloading config...\n");
            reload_config(&mut state);
            start_stopped_services(&mut state);
            state.last_config_reload_ms = now_ms;
        }

        // 5. Sleep briefly
        sleep_ms(POLL_INTERVAL_MS);
    }
}

// ============================================================================
// Config Parsing
// ============================================================================

fn parse_service_config(content: &str) -> Option<ServiceConfig> {
    let mut config = ServiceConfig::default();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();

            match key {
                "command" => config.command = String::from(value),
                "args" => {
                    config.args = value
                        .split_whitespace()
                        .map(String::from)
                        .collect();
                }
                "restart_delay" => {
                    config.restart_delay_ms = parse_u64(value).unwrap_or(DEFAULT_RESTART_DELAY_MS);
                }
                "max_retries" => {
                    config.max_retries = parse_u32(value).unwrap_or(DEFAULT_MAX_RETRIES);
                }
                "boxed" => {
                    config.boxed = value == "true" || value == "1";
                }
                "box_root" => {
                    config.box_root = String::from(value);
                }
                _ => {}
            }
        }
    }

    if config.command.is_empty() {
        return None;
    }

    Some(config)
}

fn parse_u64(s: &str) -> Option<u64> {
    let mut result: u64 = 0;
    for c in s.bytes() {
        if c < b'0' || c > b'9' {
            return None;
        }
        result = result.checked_mul(10)?.checked_add((c - b'0') as u64)?;
    }
    Some(result)
}

fn parse_u32(s: &str) -> Option<u32> {
    parse_u64(s).and_then(|v| u32::try_from(v).ok())
}

// ============================================================================
// Config Loading
// ============================================================================

fn reload_config(state: &mut HerdState) {
    // List enabled directory
    let dir = match read_dir(HERD_ENABLED_DIR) {
        Some(d) => d,
        None => {
            print("[herd] Warning: Cannot read enabled directory\n");
            return;
        }
    };

    let mut found_services: Vec<String> = Vec::new();

    for entry in dir {
        if !entry.name.ends_with(".conf") {
            continue;
        }

        let service_name = entry.name.trim_end_matches(".conf");
        found_services.push(String::from(service_name));

        // Read config file
        let config_path = format!("{}/{}", HERD_ENABLED_DIR, entry.name);
        let content = match read_file_string(&config_path) {
            Some(c) => c,
            None => continue,
        };

        // Parse config
        let config = match parse_service_config(&content) {
            Some(c) => c,
            None => {
                print("[herd] Error parsing ");
                print(&entry.name);
                print("\n");
                continue;
            }
        };

        // Update or add service
        if let Some(svc) = state.services.get_mut(service_name) {
            svc.config = config;
        } else {
            let svc = SupervisedProcess::new(String::from(service_name), config);
            state.services.insert(String::from(service_name), svc);
        }
    }

    // Remove services that are no longer enabled
    let to_remove: Vec<String> = state.services.iter()
        .filter(|(name, _)| {
            !found_services.iter().any(|n| n == *name)
        })
        .map(|(name, _)| name.clone())
        .collect();

    for name in to_remove {
        print("[herd] Stopping and removing disabled service: ");
        print(&name);
        print("\n");
        stop_service(state, &name);
        state.services.remove(&name);
    }
}

// ============================================================================
// Service Management
// ============================================================================

fn start_stopped_services(state: &mut HerdState) {
    let to_start: Vec<(String, ServiceConfig)> = state.services.iter()
        .filter(|(_, svc)| svc.state == ServiceState::Stopped)
        .map(|(name, svc)| (name.clone(), svc.config.clone()))
        .collect();

    for (name, config) in to_start {
        start_service(state, &name, &config);
    }
}

#[repr(C)]
pub struct SpawnOptions {
    pub cwd_ptr: u64,
    pub cwd_len: usize,
    pub root_dir_ptr: u64,
    pub root_dir_len: usize,
    pub box_id: u64,
}

const SYSCALL_SPAWN_EXT: u64 = 315;
const SYSCALL_REGISTER_BOX: u64 = 316;

fn start_service(state: &mut HerdState, name: &str, config: &ServiceConfig) {
    print("[herd] Starting service: ");
    print(name);
    if config.boxed {
        print(" (boxed)");
    }
    print("\n");

    // Build args
    let args: Vec<&str> = config.args.iter().map(|s| s.as_str()).collect();
    let args_opt = if args.is_empty() { None } else { Some(args.as_slice()) };

    // Spawn the process
    let spawn_res = if config.boxed {
        // Generate box_id from name
        let mut box_id = 0u64;
        for b in name.as_bytes() {
            box_id = box_id.wrapping_mul(31).wrapping_add(*b as u64);
        }
        if box_id == 0 { box_id = 1; }

        // Register box in kernel
        libakuma::syscall(
            SYSCALL_REGISTER_BOX,
            box_id,
            name.as_ptr() as u64,
            name.len() as u64,
            config.box_root.as_ptr() as u64,
            config.box_root.len() as u64,
            0,
        );

        let options = SpawnOptions {
            cwd_ptr: "/".as_ptr() as u64,
            cwd_len: 1,
            root_dir_ptr: config.box_root.as_ptr() as u64,
            root_dir_len: config.box_root.len(),
            box_id,
        };

        // Build null-separated args string for internal syscall call
        let mut args_buf = Vec::new();
        if let Some(args_slice) = args_opt {
            for arg in args_slice {
                args_buf.extend_from_slice(arg.as_bytes());
                args_buf.push(0);
            }
        }
        let args_ptr = if args_buf.is_empty() { 0 } else { args_buf.as_ptr() as u64 };
        let args_len = args_buf.len();

        let result = libakuma::syscall(
            SYSCALL_SPAWN_EXT,
            config.command.as_ptr() as u64,
            config.command.len() as u64,
            0, 0, // No stdin for now
            &options as *const _ as u64,
            0,
        );

        if (result as i64) >= 0 {
            Some(SpawnResult { 
                pid: (result & 0xFFFF_FFFF) as u32,
                stdout_fd: ((result >> 32) & 0xFFFF_FFFF) as u32
            })
        } else {
            None
        }
    } else {
        spawn(&config.command, args_opt)
    };

    match spawn_res {
        Some(SpawnResult { pid, stdout_fd }) => {
            if let Some(svc) = state.services.get_mut(name) {
                svc.pid = Some(pid);
                svc.stdout_fd = Some(stdout_fd);
                svc.state = ServiceState::Running;
                svc.restart_at_ms = None;
                print("[herd] Started ");
                print(name);
                print(" (pid=");
                print_dec(pid as usize);
                print(")\n");
            }
        }
        None => {
            print("[herd] Failed to start ");
            print(name);
            print("\n");
            if let Some(svc) = state.services.get_mut(name) {
                svc.state = ServiceState::Failed;
            }
        }
    }
}

fn stop_service(state: &mut HerdState, name: &str) {
    if let Some(svc) = state.services.get_mut(name) {
        if let Some(pid) = svc.pid {
            let _ = kill(pid);
        }
        if let Some(fd) = svc.stdout_fd {
            close(fd as i32);
        }
        svc.pid = None;
        svc.stdout_fd = None;
        svc.state = ServiceState::Stopped;
        svc.restart_at_ms = None;
    }
}

// ============================================================================
// Output Polling
// ============================================================================

fn poll_all_stdout(state: &mut HerdState) {
    let mut outputs: Vec<(String, Vec<u8>)> = Vec::new();

    for (name, svc) in state.services.iter() {
        if let Some(fd) = svc.stdout_fd {
            let mut buf = [0u8; 1024];
            let n = read_fd(fd as i32, &mut buf);
            if n > 0 {
                outputs.push((name.clone(), buf[..n as usize].to_vec()));
            }
        }
    }

    // Write to log files
    for (name, data) in outputs {
        append_to_log(state, &name, &data);
    }
}

// ============================================================================
// Exit Handling
// ============================================================================

fn check_process_exits(state: &mut HerdState, now_ms: u64) {
    let mut exited: Vec<(String, i32)> = Vec::new();

    for (name, svc) in state.services.iter() {
        if svc.state == ServiceState::Running {
            if let Some(pid) = svc.pid {
                if let Some((_, exit_code)) = waitpid(pid) {
                    exited.push((name.clone(), exit_code));
                }
            }
        }
    }

    for (name, exit_code) in exited {
        print("[herd] Service ");
        print(&name);
        print(" exited with code ");
        print_dec(exit_code as usize);
        print("\n");

        if let Some(svc) = state.services.get_mut(&name) {
            // Close stdout fd
            if let Some(fd) = svc.stdout_fd {
                close(fd as i32);
            }
            svc.pid = None;
            svc.stdout_fd = None;
            svc.last_exit_code = Some(exit_code);

            // Schedule restart on non-zero exit
            if exit_code != 0 {
                let should_restart = svc.config.max_retries == 0 
                    || svc.restart_count < svc.config.max_retries;

                if should_restart {
                    svc.restart_count += 1;
                    svc.restart_at_ms = Some(now_ms + svc.config.restart_delay_ms);
                    svc.state = ServiceState::PendingRestart;
                    print("[herd] Scheduling restart for ");
                    print(&name);
                    print("\n");
                } else {
                    svc.state = ServiceState::Failed;
                    print("[herd] Service ");
                    print(&name);
                    print(" failed after max retries\n");
                }
            } else {
                // Clean exit
                svc.state = ServiceState::Stopped;
                svc.restart_count = 0;
            }
        }
    }
}

fn process_pending_restarts(state: &mut HerdState, now_ms: u64) {
    let to_restart: Vec<(String, ServiceConfig)> = state.services.iter()
        .filter(|(_, svc)| {
            svc.state == ServiceState::PendingRestart 
                && svc.restart_at_ms.map(|t| now_ms >= t).unwrap_or(false)
        })
        .map(|(name, svc)| (name.clone(), svc.config.clone()))
        .collect();

    for (name, config) in to_restart {
        start_service(state, &name, &config);
    }
}

// ============================================================================
// Log Rotation
// ============================================================================

fn append_to_log(state: &mut HerdState, service_name: &str, data: &[u8]) {
    if data.is_empty() {
        return;
    }

    let log_path = format!("{}/{}.log", HERD_LOG_DIR, service_name);
    let log_old_path = format!("{}/{}.log.old", HERD_LOG_DIR, service_name);

    // Get current log size
    let current_size = state.services.get(service_name)
        .map(|svc| svc.log_size)
        .unwrap_or(0);

    // Check if rotation is needed
    if current_size + data.len() > MAX_LOG_SIZE {
        // Rotate: copy current to .old
        if let Some(content) = read_file_bytes(&log_path) {
            write_file(&log_old_path, &content);
        }
        
        // Write new data to log (overwrite)
        write_file(&log_path, data);

        if let Some(svc) = state.services.get_mut(service_name) {
            svc.log_size = data.len();
        }
    } else {
        // Append to log
        append_file(&log_path, data);

        if let Some(svc) = state.services.get_mut(service_name) {
            svc.log_size += data.len();
        }
    }
}

// ============================================================================
// File Helpers
// ============================================================================

fn read_file_string(path: &str) -> Option<String> {
    let bytes = read_file_bytes(path)?;
    core::str::from_utf8(&bytes).ok().map(String::from)
}

fn read_file_bytes(path: &str) -> Option<Vec<u8>> {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        return None;
    }

    let stat = match fstat(fd) {
        Ok(s) => s,
        Err(_) => {
            close(fd);
            return None;
        }
    };

    let size = stat.st_size as usize;
    let mut content = alloc::vec![0u8; size];

    lseek(fd, 0, seek_mode::SEEK_SET);
    let mut read = 0;
    while read < size {
        let n = read_fd(fd, &mut content[read..]);
        if n <= 0 {
            break;
        }
        read += n as usize;
    }

    close(fd);
    Some(content)
}

fn write_file(path: &str, data: &[u8]) -> bool {
    let fd = open(path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd < 0 {
        return false;
    }
    write_fd(fd, data);
    close(fd);
    true
}

fn append_file(path: &str, data: &[u8]) {
    let fd = open(path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_APPEND);
    if fd < 0 {
        return;
    }
    write_fd(fd, data);
    close(fd);
}

// ============================================================================
// Helpers
// ============================================================================

fn print_dec(val: usize) {
    libakuma::print_dec(val);
}

// ============================================================================
// Command Implementations
// ============================================================================

fn print_usage() {
    print("Usage: herd <command> [args]\n");
    print("\n");
    print("Commands:\n");
    print("  daemon         Run supervisor in foreground\n");
    print("  status         List enabled services\n");
    print("  add <svc>      Create a new service configuration\n");
    print("  config <svc>   Show service configuration\n");
    print("  enable <svc>   Enable a service\n");
    print("  disable <svc>  Disable a service\n");
    print("  log <svc>      Show service log\n");
    print("  help           Show this help\n");
    print("\n");
    print("Without arguments, runs as daemon in foreground.\n");
}

fn cmd_status() {
    print("Enabled services:\n");
    
    match read_dir(HERD_ENABLED_DIR) {
        Some(dir) => {
            let mut found = false;
            for entry in dir {
                if entry.name.ends_with(".conf") {
                    let name = entry.name.trim_end_matches(".conf");
                    print("  ");
                    print(name);
                    print("\n");
                    found = true;
                }
            }
            if !found {
                print("  (none)\n");
            }
        }
        None => {
            print("  Cannot read ");
            print(HERD_ENABLED_DIR);
            print("/\n");
        }
    }
}

fn cmd_add(name: &str) {
    let path = format!("{}/{}.conf", HERD_AVAILABLE_DIR, name);
    
    // Check if already exists
    if read_file_bytes(&path).is_some() {
        print("Service '");
        print(name);
        print("' already exists in ");
        print(HERD_AVAILABLE_DIR);
        print("/\n");
        return;
    }
    
    let default_config = format!(
        "# Herd Service Configuration for {}\n\
        command = /bin/{}\n\
        args = \n\
        restart_delay = {}\n\
        max_retries = {}\n",
        name, name, DEFAULT_RESTART_DELAY_MS, DEFAULT_MAX_RETRIES
    );
    
    if write_file(&path, default_config.as_bytes()) {
        print("Created service '");
        print(name);
        print("' in ");
        print(HERD_AVAILABLE_DIR);
        print("/\n");
        print("Edit this file and then run 'herd enable ");
        print(name);
        print("' to start it.\n");
    } else {
        print("Error: Failed to create service configuration at ");
        print(&path);
        print("\n");
    }
}

fn cmd_config(name: &str) {
    // Try enabled directory first
    let enabled_path = format!("{}/{}.conf", HERD_ENABLED_DIR, name);
    if let Some(content) = read_file_string(&enabled_path) {
        print("Config for '");
        print(name);
        print("' (enabled):\n\n");
        print(&content);
        if !content.ends_with('\n') {
            print("\n");
        }
        return;
    }
    
    // Try available directory
    let available_path = format!("{}/{}.conf", HERD_AVAILABLE_DIR, name);
    if let Some(content) = read_file_string(&available_path) {
        print("Config for '");
        print(name);
        print("' (not enabled):\n\n");
        print(&content);
        if !content.ends_with('\n') {
            print("\n");
        }
        return;
    }
    
    print("Service '");
    print(name);
    print("' not found.\n");
    print("Check ");
    print(HERD_AVAILABLE_DIR);
    print("/ and ");
    print(HERD_ENABLED_DIR);
    print("/\n");
}

fn cmd_enable(name: &str) {
    let src_path = format!("{}/{}.conf", HERD_AVAILABLE_DIR, name);
    let dst_path = format!("{}/{}.conf", HERD_ENABLED_DIR, name);
    
    // Check if already enabled
    if read_file_bytes(&dst_path).is_some() {
        print("Service '");
        print(name);
        print("' is already enabled.\n");
        return;
    }
    
    // Read source config
    let content = match read_file_bytes(&src_path) {
        Some(c) => c,
        None => {
            print("Service '");
            print(name);
            print("' not found in ");
            print(HERD_AVAILABLE_DIR);
            print("/\n");
            return;
        }
    };
    
    // Write to enabled
    if write_file(&dst_path, &content) {
        print("Enabled service '");
        print(name);
        print("'\n");
        print("Service will start on next config reload (within 20s) or reboot.\n");
    } else {
        print("Error: Failed to enable service '");
        print(name);
        print("'. Could not write to ");
        print(&dst_path);
        print("\n");
    }
}

fn cmd_disable(name: &str) {
    let path = format!("{}/{}.conf", HERD_ENABLED_DIR, name);
    
    // Check if enabled
    if read_file_bytes(&path).is_none() {
        print("Service '");
        print(name);
        print("' is not enabled.\n");
        return;
    }
    
    // Remove from enabled
    if libakuma::unlink(&path) == 0 {
        print("Disabled service '");
        print(name);
        print("'\n");
    } else {
        print("Error: Failed to delete ");
        print(&path);
        print("\n");
    }
}

fn cmd_log(name: &str) {
    let log_path = format!("{}/{}.log", HERD_LOG_DIR, name);
    
    match read_file_string(&log_path) {
        Some(content) => {
            if content.is_empty() {
                print("Log for '");
                print(name);
                print("' is empty.\n");
            } else {
                print(&content);
                if !content.ends_with('\n') {
                    print("\n");
                }
            }
        }
        None => {
            print("No log found for '");
            print(name);
            print("'\n");
        }
    }
}
