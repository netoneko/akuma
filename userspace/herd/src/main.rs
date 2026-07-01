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
    /// Terminal state for a `oneshot` service that has run and exited. It is never
    /// (re)started — unlike `Stopped`, which `start_stopped_services` brings back up.
    /// A reboot re-runs it (fresh herd, fresh state); a config reload preserves it.
    Completed,
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
    /// Path to an OCI bundle directory. If set, overrides command/box_root
    /// with values from the bundle's config.json.
    bundle: String,
    /// Network stack for the box: "" / "smoltcp" (default) or "rump" (route the
    /// box's AF_INET through its rump_server via the kernel sysproxy client).
    stack: String,
    /// Whether to restart the service when it exits. Default true. Set false for
    /// services whose restart needs special handling (e.g. a rump_server, whose
    /// kernel sysproxy channel must be re-established on restart — TBD).
    restart: bool,
    /// If set, spawn this service INTO an existing box (by name) instead of
    /// registering a new one. The target box must already exist and be marked
    /// stack=rump by its owner service (e.g. sshd `join_box = rumpnet` so its
    /// AF_INET routes to the rumpnet box's rump_server). When set, herd does NOT
    /// register the box or set its stack — the owner owns that.
    join_box: String,
    /// Mount points to create in the box's namespace before spawning (only
    /// "proc"/"tmpfs"). A fresh-root (box_root != "/") box has no /proc unless
    /// mounted here — sshd's interactive bridge needs /proc/<pid>/fd/0.
    mount_fs: Vec<String>,
    /// Defer the service's INITIAL start by this many ms (e.g. so a join_box
    /// service starts after its target box's rump_server has finished its
    /// handshake). Restart backstops any remaining race.
    start_delay_ms: u64,
    /// Run exactly once: start the service, and when it exits move it to the
    /// terminal `Completed` state instead of `Stopped` (so it is never restarted),
    /// regardless of exit code. Overrides `restart`. A reboot runs it again.
    oneshot: bool,
    /// Multikernel core pin (docs/MULTIKERNEL.md §10, CORE_AWARE_SCHEDULING.md). 0 =
    /// unpinned / BSP (current behavior: spawn locally on core 0). Non-zero = run this
    /// service on that secondary core's kernel: herd hands the kernel the command path in
    /// the `core_init` activation message and that core spawns it LOCALLY (no cross-core
    /// spawn). A pinned service has no local pid — it lives on its core, its output drains
    /// via that core's console ring, and its exit is reaped by that core's kernel.
    /// Mutually exclusive with boxes (see `is_boxed`).
    core: u32,
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
            bundle: String::new(),
            stack: String::new(),
            restart: true,
            join_box: String::new(),
            mount_fs: Vec::new(),
            start_delay_ms: 0,
            oneshot: false,
            core: 0,
        }
    }
}

// ============================================================================
// OCI Bundle Config Parser
// ============================================================================

/// Parsed OCI config.json (subset)
#[derive(Clone)]
struct OciConfig {
    root_path: String,
    process_args: Vec<String>,
    process_cwd: String,
    process_env: Vec<String>,
    mounts: Vec<OciMount>,
}

#[derive(Clone)]
struct OciMount {
    destination: String,
    mount_type: String,
}

impl OciConfig {
    fn default() -> Self {
        Self {
            root_path: String::from("rootfs"),
            process_args: Vec::new(),
            process_cwd: String::from("/"),
            process_env: Vec::new(),
            mounts: Vec::new(),
        }
    }
}

/// Minimal JSON string value extractor.
/// Finds `"key": "value"` and returns value.
fn json_get_str<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("\"{}\"", key);
    let idx = json.find(&pattern)?;
    let after_key = &json[idx + pattern.len()..];
    // Skip whitespace and colon
    let after_colon = after_key.trim_start().strip_prefix(':')?;
    let after_colon = after_colon.trim_start();
    if !after_colon.starts_with('"') {
        return None;
    }
    let start = 1;
    let end = after_colon[start..].find('"')?;
    Some(&after_colon[start..start + end])
}

/// Extract a JSON array of strings: `"key": ["a", "b", "c"]`
fn json_get_str_array(json: &str, key: &str) -> Vec<String> {
    let pattern = format!("\"{}\"", key);
    let idx = match json.find(&pattern) {
        Some(i) => i,
        None => return Vec::new(),
    };
    let after_key = &json[idx + pattern.len()..];
    let after_colon = match after_key.trim_start().strip_prefix(':') {
        Some(s) => s.trim_start(),
        None => return Vec::new(),
    };
    if !after_colon.starts_with('[') {
        return Vec::new();
    }
    let bracket_end = match after_colon.find(']') {
        Some(i) => i,
        None => return Vec::new(),
    };
    let array_content = &after_colon[1..bracket_end];

    let mut result = Vec::new();
    let mut remaining = array_content;
    loop {
        remaining = remaining.trim_start();
        if remaining.is_empty() {
            break;
        }
        if remaining.starts_with(',') {
            remaining = &remaining[1..];
            continue;
        }
        if remaining.starts_with('"') {
            remaining = &remaining[1..];
            if let Some(end) = remaining.find('"') {
                result.push(String::from(&remaining[..end]));
                remaining = &remaining[end + 1..];
            } else {
                break;
            }
        } else {
            break;
        }
    }
    result
}

/// Find a JSON object block by key: `"key": { ... }`
/// Returns the content between the braces.
fn json_get_object<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("\"{}\"", key);
    let idx = json.find(&pattern)?;
    let after_key = &json[idx + pattern.len()..];
    let after_colon = after_key.trim_start().strip_prefix(':')?;
    let after_colon = after_colon.trim_start();
    if !after_colon.starts_with('{') {
        return None;
    }
    let mut depth = 0;
    let mut end_idx = 0;
    for (i, ch) in after_colon.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end_idx = i;
                    break;
                }
            }
            _ => {}
        }
    }
    if end_idx > 0 {
        Some(&after_colon[1..end_idx])
    } else {
        None
    }
}

/// Extract the `"mounts"` array from config.json.
/// Each mount is `{ "destination": "...", "type": "...", ... }`
fn json_get_mounts(json: &str) -> Vec<OciMount> {
    let pattern = "\"mounts\"";
    let idx = match json.find(pattern) {
        Some(i) => i,
        None => return Vec::new(),
    };
    let after_key = &json[idx + pattern.len()..];
    let after_colon = match after_key.trim_start().strip_prefix(':') {
        Some(s) => s.trim_start(),
        None => return Vec::new(),
    };
    if !after_colon.starts_with('[') {
        return Vec::new();
    }

    let mut mounts = Vec::new();
    let mut remaining = &after_colon[1..]; // skip '['

    loop {
        remaining = remaining.trim_start();
        if remaining.is_empty() || remaining.starts_with(']') {
            break;
        }
        if remaining.starts_with(',') {
            remaining = &remaining[1..];
            continue;
        }
        if remaining.starts_with('{') {
            let mut depth = 0;
            let mut end_idx = 0;
            for (i, ch) in remaining.char_indices() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end_idx = i;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if end_idx > 0 {
                let obj = &remaining[1..end_idx];
                let dest = json_get_str(obj, "destination")
                    .map(String::from)
                    .unwrap_or_default();
                let mtype = json_get_str(obj, "type")
                    .map(String::from)
                    .unwrap_or_default();
                if !dest.is_empty() && !mtype.is_empty() {
                    mounts.push(OciMount {
                        destination: dest,
                        mount_type: mtype,
                    });
                }
                remaining = &remaining[end_idx + 1..];
            } else {
                break;
            }
        } else {
            break;
        }
    }

    mounts
}

/// Parse an OCI config.json string into an OciConfig.
fn parse_oci_config(json: &str) -> OciConfig {
    let mut config = OciConfig::default();

    if let Some(root_obj) = json_get_object(json, "root") {
        if let Some(path) = json_get_str(root_obj, "path") {
            config.root_path = String::from(path);
        }
    }

    if let Some(proc_obj) = json_get_object(json, "process") {
        config.process_args = json_get_str_array(proc_obj, "args");
        config.process_env = json_get_str_array(proc_obj, "env");
        if let Some(cwd) = json_get_str(proc_obj, "cwd") {
            config.process_cwd = String::from(cwd);
        }
    }

    config.mounts = json_get_mounts(json);

    config
}

// ============================================================================
// Supervised Process
// ============================================================================

struct SupervisedProcess {
    config: ServiceConfig,
    pid: Option<u32>,
    stdout_fd: Option<u32>,
    state: ServiceState,
    restart_count: u32,
    last_exit_code: Option<i32>,
    restart_at_ms: Option<u64>,
    /// Earliest time (ms) this service's INITIAL start is allowed, computed lazily
    /// from `config.start_delay_ms` the first time we consider starting it.
    start_at_ms: Option<u64>,
    log_size: usize,
}

impl SupervisedProcess {
    fn new(_name: String, config: ServiceConfig) -> Self {
        Self {
            config,
            pid: None,
            stdout_fd: None,
            state: ServiceState::Stopped,
            restart_count: 0,
            last_exit_code: None,
            restart_at_ms: None,
            start_at_ms: None,
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
    /// Secondary core -> name of the service pinned there. A kernel can run only ONE init
    /// program per core (core_init overwrites the pending program), so herd must reject a
    /// second service pinned to an already-claimed core rather than silently clobber it.
    pinned_cores: BTreeMap<u32, String>,
}

impl HerdState {
    fn new() -> Self {
        Self {
            services: BTreeMap::new(),
            last_config_reload_ms: 0,
            pinned_cores: BTreeMap::new(),
        }
    }
}

// ============================================================================
// Entry Point
// ============================================================================

#[no_mangle]
pub extern "C" fn main() {
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
                exit(0);
            }
            "add" => {
                if let Some(name) = service_name {
                    cmd_add(name);
                } else {
                    print("Usage: herd add <service>\n");
                }
                exit(0);
            }
            "config" => {
                if let Some(name) = service_name {
                    cmd_config(name);
                } else {
                    print("Usage: herd config <service>\n");
                }
                exit(0);
            }
            "enable" => {
                if let Some(name) = service_name {
                    cmd_enable(name);
                } else {
                    print("Usage: herd enable <service>\n");
                }
                exit(0);
            }
            "disable" => {
                if let Some(name) = service_name {
                    cmd_disable(name);
                } else {
                    print("Usage: herd disable <service>\n");
                }
                exit(0);
            }
            "log" => {
                if let Some(name) = service_name {
                    cmd_log(name);
                } else {
                    print("Usage: herd log <service>\n");
                }
                exit(0);
            }
            "help" | "--help" | "-h" => {
                print_usage();
                exit(0);
            }
            _ => {
                print("Unknown command: ");
                print(subcommand);
                print("\n");
                print_usage();
                exit(1);
            }
        }
    }

    // Daemon mode - run supervisor loop
    print("[herd] Userspace supervisor starting...\n");

    let mut state = HerdState::new();

    // Initial config load
    reload_config(&mut state);

    // Start enabled services
    start_stopped_services(&mut state, uptime() / 1000);

    // Main supervisor loop
    supervisor_loop(state);
}

fn supervisor_loop(mut state: HerdState) {
    loop {
        let now_ms = uptime() / 1000; // uptime() returns microseconds

        // 1. Poll stdout from running services
        poll_all_stdout(&mut state);

        // 2. Check for exited processes
        check_process_exits(&mut state, now_ms);

        // 3. Handle pending restarts
        process_pending_restarts(&mut state, now_ms);

        // 3b. Start any stopped services whose (optional) start delay has elapsed.
        start_stopped_services(&mut state, now_ms);

        // 4. Reload config every 20 seconds
        if now_ms.saturating_sub(state.last_config_reload_ms) >= CONFIG_RELOAD_INTERVAL_MS {
            print("[herd] Reloading config...\n");
            reload_config(&mut state);
            start_stopped_services(&mut state, now_ms);
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
                "bundle" => {
                    config.bundle = String::from(value);
                    config.boxed = true; // bundles are always boxed
                }
                "stack" => config.stack = String::from(value),
                "restart" => config.restart = value != "false" && value != "0" && value != "no",
                "join_box" => {
                    config.join_box = String::from(value);
                    config.boxed = true; // a joined service always runs in a box
                }
                "mount" => {
                    config.mount_fs = value
                        .split_whitespace()
                        .map(String::from)
                        .collect();
                }
                "start_delay" => {
                    config.start_delay_ms = parse_u64(value).unwrap_or(0);
                }
                "oneshot" => {
                    config.oneshot = value == "true" || value == "1";
                }
                "core" => {
                    config.core = parse_u64(value).unwrap_or(0) as u32;
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
        if !c.is_ascii_digit() {
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

fn start_stopped_services(state: &mut HerdState, now_ms: u64) {
    // Honor a per-service initial start delay (e.g. a join_box service waiting for
    // its target box's rump_server handshake). start_at_ms is set lazily the first
    // time we see the service so a 0-delay service still starts immediately.
    let mut to_start: Vec<(String, ServiceConfig)> = Vec::new();
    for (name, svc) in state.services.iter_mut() {
        if svc.state != ServiceState::Stopped {
            continue;
        }
        if svc.config.start_delay_ms > 0 {
            let eligible = *svc.start_at_ms.get_or_insert(now_ms + svc.config.start_delay_ms);
            if now_ms < eligible {
                continue; // not yet — wait out the delay
            }
        }
        to_start.push((name.clone(), svc.config.clone()));
    }

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
    pub args_ptr: u64,
    pub args_len: usize,
    pub stdin_ptr: u64,
    pub stdin_len: usize,
    pub box_id: u64,
}

const SYSCALL_SPAWN_EXT: u64 = 315;
const SYSCALL_REGISTER_BOX: u64 = 316;
const SYSCALL_SET_BOX_STACK: u64 = 324;
const SYSCALL_CORE_INIT: u64 = 327;

/// Multikernel: activate secondary core `idx` to run `program` as its init process
/// (docs/MULTIKERNEL.md §6/§10, acceptance/12). This is herd, the init system, owning
/// core activation: it hands the kernel the program path in the activation message
/// (`MSG_CORE_INIT`), and that core spawns it LOCALLY (its ELF fetched via forwarded
/// `open`/`read`). There is deliberately NO cross-core spawn (§7) — the process is never
/// injected into the core; the core's own kernel creates it. Returns true on success.
///
/// `program` must be a `/`-rooted path; it is passed NUL-terminated (the `core_init`
/// syscall reads it like any path). On a single-kernel boot the syscall returns `-ENOSYS`
/// and this returns false.
fn core_init(idx: u32, program: &str) -> bool {
    let mut path = Vec::with_capacity(program.len() + 1);
    path.extend_from_slice(program.as_bytes());
    path.push(0); // NUL-terminate for the kernel's path copy
    let r = libakuma::syscall(
        SYSCALL_CORE_INIT,
        idx as u64,
        path.as_ptr() as u64,
        0,
        0,
        0,
        0,
    );
    (r as i64) == 0
}

/// Whether a service config asks for any kind of box/namespace. A boxed service cannot
/// also be pinned to a non-BSP core (boxes are per-kernel-private state; see
/// userspace/herd/docs/CORE_AWARE_SCHEDULING.md) — herd rejects that combination.
fn is_boxed(config: &ServiceConfig) -> bool {
    config.boxed
        || !config.bundle.is_empty()
        || !config.join_box.is_empty()
        || config.box_root != "/"
}

/// Tell the kernel a box uses the NetBSD rump network stack (stack = 1). The
/// kernel then routes that box's AF_INET syscalls to its rump_server.
fn set_box_stack_rump(box_id: u64) {
    libakuma::syscall(SYSCALL_SET_BOX_STACK, box_id, 1, 0, 0, 0, 0);
}

fn generate_box_id(name: &str) -> u64 {
    let mut box_id = 0u64;
    for b in name.as_bytes() {
        box_id = box_id.wrapping_mul(31).wrapping_add(*b as u64);
    }
    if box_id == 0 { box_id = 1; }
    box_id
}

fn register_box(name: &str, box_id: u64, root_dir: &str, primary_pid: u32) {
    libakuma::syscall(
        SYSCALL_REGISTER_BOX,
        box_id,
        name.as_ptr() as u64,
        name.len() as u64,
        root_dir.as_ptr() as u64,
        root_dir.len() as u64,
        primary_pid as u64,
    );
}

fn spawn_in_box(
    box_id: u64,
    command: &str,
    args: &[&str],
) -> Option<SpawnResult> {
    // Match the kernel SPAWN_EXT ABI (and box/main.rs): path is NUL-terminated,
    // args is an argv POINTER ARRAY ([path\0, arg\0…, null]) — NOT a flat
    // null-separated buffer — and `options` is arg2 (not arg3). The previous
    // flat-buffer + arg3 layout made the kernel read `command.len()` as the
    // options pointer → EFAULT (boxed services never started).
    let path_term = format!("{}\0", command);
    let args_term: Vec<String> = args.iter().map(|a| format!("{}\0", a)).collect();
    let mut argv: Vec<*const u8> = Vec::with_capacity(args_term.len() + 2);
    argv.push(path_term.as_ptr());
    for s in &args_term {
        argv.push(s.as_ptr());
    }
    argv.push(core::ptr::null());

    let options = SpawnOptions {
        cwd_ptr: "/".as_ptr() as u64,
        cwd_len: 1,
        root_dir_ptr: 0,
        root_dir_len: 0,
        args_ptr: argv.as_ptr() as u64,
        args_len: argv.len(),
        stdin_ptr: 0,
        stdin_len: 0,
        box_id,
    };

    let result = libakuma::syscall(
        SYSCALL_SPAWN_EXT,
        path_term.as_ptr() as u64,
        &options as *const _ as u64,
        0,
        0,
        0,
        0,
    );

    if (result as i64) >= 0 {
        let pid = (result & 0xFFFF_FFFF) as u32;
        let stdout_fd = ((result >> 32) & 0xFFFF_FFFF) as u32;
        Some(SpawnResult { pid, stdout_fd })
    } else {
        None
    }
}

const SYSCALL_MOUNT_IN_NS: u64 = 325;

/// Set up mounts in a box's namespace from OCI config mount entries.
fn setup_oci_mounts(box_id: u64, mounts: &[OciMount]) {
    for m in mounts {
        match m.mount_type.as_str() {
            "proc" | "tmpfs" => {}
            _ => continue,
        };

        let result = libakuma::syscall(
            SYSCALL_MOUNT_IN_NS,
            box_id,
            m.destination.as_ptr() as u64,
            m.destination.len() as u64,
            m.mount_type.as_ptr() as u64,
            m.mount_type.len() as u64,
            0,
        );

        if (result as i64) < 0 {
            print("[herd] Warning: Failed to mount ");
            print(&m.mount_type);
            print(" at ");
            print(&m.destination);
            print("\n");
        }
    }
}

/// Mount the configured `mount` filesystems into a box's namespace. Each entry is
/// a type ("proc"/"tmpfs") mounted at its conventional path. A fresh-root box
/// (box_root != "/") otherwise has no /proc — sshd's interactive bridge needs it.
fn setup_fs_mounts(box_id: u64, mounts: &[String]) {
    for m in mounts {
        let (fstype, dest): (&str, &str) = match m.as_str() {
            "proc" => ("proc", "/proc"),
            "tmpfs" => ("tmpfs", "/tmp"),
            _ => continue,
        };
        let result = libakuma::syscall(
            SYSCALL_MOUNT_IN_NS,
            box_id,
            dest.as_ptr() as u64,
            dest.len() as u64,
            fstype.as_ptr() as u64,
            fstype.len() as u64,
            0,
        );
        if (result as i64) < 0 {
            print("[herd] Warning: Failed to mount ");
            print(fstype);
            print(" at ");
            print(dest);
            print(" in box\n");
        }
    }
}

fn start_service(state: &mut HerdState, name: &str, config: &ServiceConfig) {
    // Core-pinned service (multikernel): don't spawn locally. Hand the command to the
    // target core's kernel via core_init — the activation message carries the program
    // path and that core spawns it LOCALLY (ELF fetched via forwarded open/read). There
    // is no cross-core spawn and no local pid to supervise: the process lives on core N,
    // its stdout drains to the console through core N's ring, and its exit is reaped by
    // core N's kernel (docs/MULTIKERNEL.md §7/§10, acceptance/12 Milestone 2).
    if config.core != 0 {
        print("[herd] Starting service: ");
        print(name);
        print(" on core ");
        print_dec(config.core as usize);
        print("\n");
        // Box + non-BSP core is a misconfiguration: boxes are per-kernel-private state, so
        // a BSP box can't follow a process onto a secondary (CORE_AWARE_SCHEDULING.md).
        if is_boxed(config) {
            print("[herd] Error: ");
            print(name);
            print(": box + non-BSP core is unsupported — not started\n");
            if let Some(svc) = state.services.get_mut(name) {
                svc.state = ServiceState::Failed;
            }
            return;
        }
        // One init program per core: the kernel's core_init overwrites the pending program,
        // so if another service already claimed this core, reject this one with an error
        // rather than silently clobbering it (which is what happened when sshd and netcheck
        // were both pinned to core 1). A service re-pinning the SAME core it already owns
        // (e.g. across a config reload) is fine.
        if let Some(existing) = state.pinned_cores.get(&config.core) {
            if existing.as_str() != name {
                print("[herd] Error: ");
                print(name);
                print(": core ");
                print_dec(config.core as usize);
                print(" already pinned to '");
                print(existing);
                print("' — only one init program per core; not started\n");
                if let Some(svc) = state.services.get_mut(name) {
                    svc.state = ServiceState::Failed;
                }
                return;
            }
        }
        // The activation message carries the WHOLE command line (program + args), space-
        // separated; the target core's kernel splits it back into argv before spawning (the
        // pinned process gets its arguments, e.g. `curl -sS https://ifconfig.me`).
        let mut cmdline = config.command.clone();
        for a in &config.args {
            cmdline.push(' ');
            cmdline.push_str(a);
        }
        let ok = core_init(config.core, &cmdline);
        if ok {
            state.pinned_cores.insert(config.core, String::from(name));
        }
        if let Some(svc) = state.services.get_mut(name) {
            if ok {
                print("[herd] core_init(");
                print_dec(config.core as usize);
                print(") requested: ");
                print(&cmdline);
                print("\n");
                // No local pid — the process runs on the core. A oneshot pinned service
                // goes terminal (Completed) immediately (it runs once on its core, and
                // herd can't waitpid a cross-core process); otherwise mark Running
                // best-effort (not locally supervised). Restart is not attempted for
                // pinned services in this cut.
                svc.pid = None;
                svc.stdout_fd = None;
                svc.state = if config.oneshot {
                    ServiceState::Completed
                } else {
                    ServiceState::Running
                };
            } else {
                print("[herd] core_init failed for ");
                print(name);
                print(" — not started\n");
                svc.state = ServiceState::Failed;
            }
        }
        return;
    }

    print("[herd] Starting service: ");
    print(name);
    if !config.bundle.is_empty() {
        print(" (bundle: ");
        print(&config.bundle);
        print(")");
    } else if config.boxed {
        print(" (boxed)");
    }
    print("\n");

    let spawn_res = if !config.join_box.is_empty() {
        // Join an EXISTING box (e.g. sshd into the rumpnet box so its AF_INET is
        // sysproxy-routed to that box's rump_server). The target box was registered
        // and stack-marked by its owner service — do NOT register_box or
        // set_box_stack_rump here. We mount the box's namespace fs (e.g. /proc for
        // sshd's stdin bridge) then spawn into the existing box id. If the owner
        // hasn't registered the box yet, spawn_in_box falls back to the caller ns
        // and the mount fails — `start_delay` + `restart` cover that race.
        let target_box_id = generate_box_id(&config.join_box);
        setup_fs_mounts(target_box_id, &config.mount_fs);
        let args: Vec<&str> = config.args.iter().map(|s| s.as_str()).collect();
        spawn_in_box(target_box_id, &config.command, &args)
    } else if !config.bundle.is_empty() {
        // OCI Bundle mode
        let config_path = format!("{}/config.json", config.bundle);
        let json = match read_file_string(&config_path) {
            Some(s) => s,
            None => {
                print("[herd] Error: Cannot read ");
                print(&config_path);
                print("\n");
                if let Some(svc) = state.services.get_mut(name) {
                    svc.state = ServiceState::Failed;
                }
                return;
            }
        };

        let oci = parse_oci_config(&json);

        let root_dir = if oci.root_path.starts_with('/') {
            oci.root_path.clone()
        } else {
            format!("{}/{}", config.bundle, oci.root_path)
        };

        let box_id = generate_box_id(name);

        let command = if !oci.process_args.is_empty() {
            oci.process_args[0].clone()
        } else if !config.command.is_empty() {
            config.command.clone()
        } else {
            print("[herd] Error: No command in OCI config or service config\n");
            if let Some(svc) = state.services.get_mut(name) {
                svc.state = ServiceState::Failed;
            }
            return;
        };

        let args: Vec<&str> = oci.process_args.iter().skip(1).map(|s| s.as_str()).collect();

        // 1. Register box (creates mount namespace in kernel)
        register_box(name, box_id, &root_dir, 0);
        if config.stack == "rump" {
            set_box_stack_rump(box_id);
        }

        // 2. Set up OCI mounts in the box's namespace
        setup_oci_mounts(box_id, &oci.mounts);

        // 3. Spawn the main process (namespace handles path resolution)
        let res = spawn_in_box(box_id, &command, &args);
        if let Some(ref r) = res {
            register_box(name, box_id, &root_dir, r.pid);
        }
        res
    } else if config.boxed {
        let box_id = generate_box_id(name);
        let args: Vec<&str> = config.args.iter().map(|s| s.as_str()).collect();
        register_box(name, box_id, &config.box_root, 0);
        if config.stack == "rump" {
            // Mark the box BEFORE spawning so the kernel knows this box's
            // rump_server should get a sysproxy channel wired onto fd 3 when we
            // spawn it below. herd owns the rump_server lifecycle (one server,
            // no second kernel-spawned one); the kernel only attaches the
            // channel + drives the proxy.
            set_box_stack_rump(box_id);
        }
        setup_fs_mounts(box_id, &config.mount_fs);
        let res = spawn_in_box(box_id, &config.command, &args);
        if let Some(ref r) = res {
            register_box(name, box_id, &config.box_root, r.pid);
        }
        res
    } else {
        let args: Vec<&str> = config.args.iter().map(|s| s.as_str()).collect();
        let args_opt = if args.is_empty() { None } else { Some(args.as_slice()) };
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

            // A oneshot service ran its single time: move it to the terminal
            // Completed state (never restarted — start_stopped_services only revives
            // Stopped), regardless of exit code. A reboot runs it again.
            if svc.config.oneshot {
                svc.state = ServiceState::Completed;
                svc.restart_count = 0;
                print("[herd] Oneshot service ");
                print(&name);
                print(" completed\n");
            } else if exit_code != 0 && svc.config.restart {
                // Schedule restart on non-zero exit (unless restart is disabled).
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

    // st_size is only a HINT: procfs files (e.g. /proc/cores) report size 0 yet stream
    // real bytes on read, so we must read until EOF rather than trusting the stat size.
    // Use the stat size to pre-size the buffer when it is nonzero; otherwise grow in
    // CHUNK-sized steps.
    const CHUNK: usize = 4096;
    let hint = stat.st_size as usize;
    let mut content: Vec<u8> = Vec::new();
    lseek(fd, 0, seek_mode::SEEK_SET);
    let mut read = 0usize;
    loop {
        // Ensure at least CHUNK bytes of spare capacity to read into.
        let want = (hint.max(read + CHUNK)).max(content.len());
        content.resize(want, 0);
        let n = read_fd(fd, &mut content[read..]);
        if n <= 0 {
            break;
        }
        read += n as usize;
    }
    content.truncate(read);

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
