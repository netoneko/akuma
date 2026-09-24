//! Herd - Userspace Process Supervisor
//!
//! A process supervisor that manages background services.
//! Named "herd" because herding cats is an apt metaphor for managing processes.

// std and the test harness's own entry under `cargo test --bin herd`; see the
// `[dev-dependencies]` note in Cargo.toml.
#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
#![cfg_attr(test, allow(dead_code))]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::{
    print, exit, open, read_fd, write_fd, close, fstat, lseek,
    open_flags, seek_mode, spawn, spawn_with_env, kill_signal, waitpid_status, wait_any, read_dir,
    uptime, sleep_ms, mkdir_p, SpawnResult, SIGKILL, SIGTERM,
};
use libakuma::net::{ErrorKind, TcpListener, TcpStream};

use boxlib::json;
use boxlib::spec::{self, box_id_for};
use boxlib::sys;

use herd::exit::{classify, Exit, Outcome, Policy};

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
/// `herd start`/`herd stop` reach the running daemon over this loopback TCP
/// socket: one request line in (`stop kot\n`), one reply line back
/// (`ok stopped kot (pid 20): killed by signal 15\n` or `err …`), then close.
/// The CLI waits for the reply, so it reports what actually happened instead of
/// "requested".
///
/// Loopback TCP rather than an AF_UNIX path because the amd64 kernel has no
/// AF_UNIX (`amd64/src/sock.rs::sys_socket` answers `EAFNOSUPPORT`) and herd
/// runs on both kernels.
///
/// This replaced `/etc/herd/control/<svc>.{start,stop}` marker files, which the
/// daemon listed on every 100 ms tick — `openat`/`fstat`/`getdents64`/`close`
/// for a directory that was almost always empty — and which could not carry an
/// answer back. The listener is non-blocking, so an idle tick costs one
/// `accept` returning `EAGAIN`.
const HERD_CONTROL_ADDR: &str = "127.0.0.1:7117";

/// How long a stopped service gets to exit after SIGTERM before SIGKILL.
const STOP_TERM_GRACE_MS: u64 = 3_000;

/// How long after SIGKILL herd keeps trying to reap before giving up and
/// leaving the pid to the orphan sweep (see [`SupervisedProcess::stopping_pid`]).
const STOP_KILL_GRACE_MS: u64 = 2_000;

/// Reap-poll interval while a stop waits for the process to exit.
const STOP_POLL_MS: u64 = 20;

/// How long a control connection gets to send its request line.
const CONTROL_READ_TIMEOUT_MS: u64 = 1_000;

/// Upper bound on control connections served per supervisor tick.
const MAX_CONTROL_REQUESTS_PER_TICK: usize = 8;

/// The enabled-config directory the daemon loads from. Defaults to
/// [`HERD_ENABLED_DIR`], but a `--enabled-dir <path>` argument overrides it — so a SECOND herd
/// instance (e.g. pinned to a secondary core to bring up that core's own rump box) can run a
/// distinct service set without touching the BSP herd's `/etc/herd/enabled`.
fn enabled_dir() -> String {
    let mut i = 1;
    loop {
        match libakuma::arg(i) {
            Some("--enabled-dir") => {
                return libakuma::arg(i + 1).map_or_else(|| String::from(HERD_ENABLED_DIR), String::from);
            }
            Some(_) => i += 1,
            None => return String::from(HERD_ENABLED_DIR),
        }
    }
}
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
    /// Administratively stopped via `herd stop <svc>`. Unlike `Stopped` — which
    /// `start_stopped_services` revives on the very next pass — a `Halted` service
    /// stays down until `herd start <svc>` (or a reboot, which starts from fresh
    /// state) brings it back. Config reload does not clear it: a service disabled
    /// via `herd stop` should not come back just because the 20s reload ran.
    Halted,
    /// Ran, exited, and its policy says not to restart it — a clean exit, or any
    /// exit with `restart = false` ([`Outcome::Stopped`]). Stays down until
    /// `herd start <svc>` or a reboot; a config reload preserves it.
    ///
    /// This used to land in `Stopped`, which means "not started yet", so
    /// `start_stopped_services` launched it again on the very next pass with no
    /// delay at all: a `restart = false` one-shot ran ~700 times in 85 s
    /// (2026-09-24, `docs/archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md` §6 item 6).
    Exited,
}

impl ServiceState {
    /// The state an exit's [`Outcome`] puts a service in.
    const fn after(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Completed => Self::Completed,
            Outcome::Restart => Self::PendingRestart,
            Outcome::Failed => Self::Failed,
            Outcome::Stopped => Self::Exited,
        }
    }

    /// Whether [`start_stopped_services`] launches a service in this state on
    /// its next pass — "not started yet", and nothing else.
    const fn started_by_start_pass(self) -> bool {
        matches!(self, Self::Stopped)
    }
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
    /// Environment variables for the service, one `env = KEY=VALUE` line each.
    ///
    /// Repeatable rather than whitespace-separated like `args`, so a value may
    /// contain spaces. These **override by name**: for a `bundle` service the
    /// base is the bundle's own `process.env` (the OCI runtime spec's
    /// environment), otherwise it is `boxlib::spec::DEFAULT_ENV`.
    ///
    /// Left empty, nothing is composed at all and the spawn passes no
    /// environment, which is what makes the kernel apply its own default — so an
    /// existing service's environment is byte-identical to before this existed.
    env: Vec<String>,
    /// Working directory for the service, or empty to inherit herd's (`/`).
    ///
    /// Akuma's `SPAWN` syscall carries no working directory, so a child gets
    /// whatever herd's own is — and for a supervisor started as `init` that is
    /// `/`. Some programs care a great deal: llama.cpp scans for its ggml
    /// backend libraries relative to the working directory, and from `/` that
    /// walk descends into whatever large trees the root holds. Measured
    /// 2026-09-20 on the bare-metal box, `llama-server` issued 4480 `stat` and
    /// 140 `getdents64` calls while opening its model **zero** times, then sat
    /// there with no listener and one second of CPU. It reads exactly like a
    /// model loading slowly, and it is a directory walk.
    ///
    /// Passed through `SPAWN_EXT`'s `SpawnOptions.cwd`, which is the only spawn
    /// that carries one. A `chdir` in herd before a plain `spawn` does *not*
    /// work — the child does not inherit our cwd, the kernel assigns it the
    /// one in the options (`/` by default) — and the failure is silent, so it
    /// is worth stating: a `pwd` oneshot printed `/` either way.
    workdir: String,
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
            env: Vec::new(),
            stack: String::new(),
            restart: true,
            join_box: String::new(),
            mount_fs: Vec::new(),
            start_delay_ms: 0,
            oneshot: false,
            workdir: String::new(),
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

/// Extract the `"mounts"` array from config.json: each mount is
/// `{ "destination": "...", "type": "...", ... }`. `destination` and `type`
/// are correlated by array position rather than assumed adjacent, since a mount
/// object can carry other members (`options`, `source`) between them.
fn oci_mounts(doc: &str) -> Vec<OciMount> {
    let mut dest: BTreeMap<usize, String> = BTreeMap::new();
    let mut mtype: BTreeMap<usize, String> = BTreeMap::new();
    let _ = json::walk(doc, |path, value| {
        let json::Value::Str(s) = value else { return };
        if let Some(i) = path.index_at(1) {
            if path.matches(&["mounts", "*", "destination"]) {
                dest.insert(i, String::from(s));
            } else if path.matches(&["mounts", "*", "type"]) {
                mtype.insert(i, String::from(s));
            }
        }
    });
    dest.into_iter()
        .filter_map(|(i, destination)| {
            mtype.get(&i).map(|mount_type| OciMount {
                destination,
                mount_type: mount_type.clone(),
            })
        })
        .collect()
}

/// Parse an OCI config.json string into an OciConfig.
fn parse_oci_config(doc: &str) -> OciConfig {
    let mut config = OciConfig::default();

    if let Some(path) = json::string_at(doc, &["root", "path"]) {
        config.root_path = path;
    }

    config.process_args = json::strings_at(doc, &["process", "args", "*"]);
    config.process_env = json::strings_at(doc, &["process", "env", "*"]);
    if let Some(cwd) = json::string_at(doc, &["process", "cwd"]) {
        config.process_cwd = cwd;
    }

    config.mounts = oci_mounts(doc);

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
    /// A pid `herd stop` signalled but could not reap, even after SIGKILL — a
    /// SIGTERM'd multi-threaded kot on amd64 has stayed un-reapable for minutes
    /// (`docs/archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md` §6 item 7). The
    /// orphan sweep clears it when the pid is finally reaped; until then
    /// `herd start` refuses, because a second copy beside a half-dead first
    /// (same port, same on-disk store) is worse than no copy.
    stopping_pid: Option<u32>,
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
            stopping_pid: None,
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
    /// The `herd start`/`herd stop` listener on [`HERD_CONTROL_ADDR`], or
    /// `None` if it could not be opened (the daemon still supervises).
    control: Option<TcpListener>,
}

impl HerdState {
    fn new() -> Self {
        Self {
            services: BTreeMap::new(),
            last_config_reload_ms: 0,
            pinned_cores: BTreeMap::new(),
            control: None,
        }
    }
}

// ============================================================================
// Entry Point
// ============================================================================

#[cfg_attr(not(test), no_mangle)]
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
            "start" => {
                let Some(name) = service_name else {
                    print("Usage: herd start <service>\n");
                    exit(1);
                };
                exit(if cmd_start(name) { 0 } else { 1 });
            }
            "stop" => {
                let Some(name) = service_name else {
                    print("Usage: herd stop <service>\n");
                    exit(1);
                };
                exit(if cmd_stop(name) { 0 } else { 1 });
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

    state.control = open_control_socket();

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

        // 3a. Answer any `herd start`/`herd stop` connections since the last tick.
        serve_control(&mut state);

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
                // Both spellings: the `_ms` form is what the reference docs and
                // the shipped devbox `sshd.conf` have always written, and it was
                // silently falling through to `_ => {}` — so that config's
                // 10-second wait for box 0's rump DHCP handshake never happened.
                "restart_delay" | "restart_delay_ms" => {
                    config.restart_delay_ms = value.parse::<u64>().ok().unwrap_or(DEFAULT_RESTART_DELAY_MS);
                }
                "max_retries" => {
                    config.max_retries = value.parse::<u32>().ok().unwrap_or(DEFAULT_MAX_RETRIES);
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
                "start_delay" | "start_delay_ms" => {
                    config.start_delay_ms = value.parse::<u64>().ok().unwrap_or(0);
                }
                "oneshot" => {
                    config.oneshot = value == "true" || value == "1";
                }
                // Both spellings, because both are the obvious one to reach for.
                "workdir" | "working_dir" => config.workdir = String::from(value),
                // Repeatable: each line adds one variable. The `split_once('=')`
                // above takes the FIRST `=`, so the value keeps any further ones
                // (`env = DSN=host=db port=5432` is one entry, spaces and all).
                "env" => {
                    if !value.is_empty() {
                        config.env.push(String::from(value));
                    }
                }
                "core" => {
                    config.core = value.parse::<u64>().ok().unwrap_or(0) as u32;
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

// ============================================================================
// Config Loading
// ============================================================================

/// If `--service <path>` args were given, read each config file DIRECTLY (`openat`+`read`, which
/// forwards fine to the VFS owner on a secondary core — unlike a directory *listing*, which
/// isn't forwarded). Returns `(service_name, content)` pairs (name = basename minus `.conf`), or
/// `None` to fall back to an enabled-dir scan. Lets a per-core herd (e.g. pinned to core 2 to
/// bring up that core's own rump box) load its services without ever scanning a directory.
fn explicit_service_files() -> Option<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut saw_service_flag = false;
    let mut i = 1;
    loop {
        match libakuma::arg(i) {
            Some("--service") => {
                saw_service_flag = true;
                if let Some(path) = libakuma::arg(i + 1) {
                    let base = path.rsplit('/').next().unwrap_or(path);
                    let name = base.trim_end_matches(".conf");
                    match read_file_string(path) {
                        Some(c) => out.push((String::from(name), c)),
                        None => {
                            print("[herd] Warning: cannot read --service ");
                            print(path);
                            print("\n");
                        }
                    }
                }
                i += 2;
            }
            Some(_) => i += 1,
            None => break,
        }
    }
    // If ANY `--service` flag was given, this is an explicit-config sub-herd (e.g. a
    // core-pinned instance): honor exactly that set, even if empty/unreadable. Only fall
    // back to the enabled-dir scan when NO `--service` flag was seen at all. Falling back
    // here when a sub-herd's `--service` files are missing would make it re-scan the BSP's
    // enabled dir — which lists this very sub-herd (command=/bin/herd) and re-launches it,
    // an unbounded recursive herd fork-bomb.
    if saw_service_flag {
        Some(out)
    } else {
        None
    }
}

fn reload_config(state: &mut HerdState) {
    // Source the service configs: explicit `--service <path>` files (read directly), else scan
    // the enabled dir (honors --enabled-dir). Direct files work on a secondary core, where the
    // VFS is forwarded to core 0 and file reads work but directory listing does not.
    let entries: Vec<(String, String)> = match explicit_service_files() {
        Some(list) => list,
        None => {
            let ed = enabled_dir();
            let dir = match read_dir(&ed) {
                Some(d) => d,
                None => {
                    print("[herd] Warning: Cannot read enabled directory\n");
                    return;
                }
            };
            let mut v: Vec<(String, String)> = Vec::new();
            for entry in dir {
                if !entry.name.ends_with(".conf") {
                    continue;
                }
                let name = String::from(entry.name.trim_end_matches(".conf"));
                let path = format!("{}/{}", ed, entry.name);
                if let Some(c) = read_file_string(&path) {
                    v.push((name, c));
                }
            }
            v
        }
    };

    let mut found_services: Vec<String> = Vec::new();
    for (service_name, content) in &entries {
        found_services.push(service_name.clone());

        // Parse config
        let config = match parse_service_config(content) {
            Some(c) => c,
            None => {
                print("[herd] Error parsing ");
                print(service_name);
                print("\n");
                continue;
            }
        };

        // Update or add service
        if let Some(svc) = state.services.get_mut(service_name) {
            svc.config = config;
        } else {
            let svc = SupervisedProcess::new(service_name.clone(), config);
            state.services.insert(service_name.clone(), svc);
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
        let reply = stop_service(state, &name);
        print("[herd] ");
        print(&reply.msg);
        print("\n");
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
        if !svc.state.started_by_start_pass() {
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
/// and this returns false. Also returns false on shared-kernel SMP where CORE_INIT is
/// not available (all cores are already online and run the same kernel).
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
    // Success (0) or ENOSYS (-38) means core_init is not available in this kernel mode
    // In shared-kernel SMP, we fall back to normal spawning
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

/// Compose a service's environment, or `None` to leave it to the kernel.
///
/// `base` is the bundle's `process.env` for an OCI bundle service and empty
/// otherwise. Returning `None` when nothing was configured is deliberate: the
/// kernel applies `DEFAULT_ENV` only to a spawn that passes no environment, so
/// composing unconditionally would change every existing service's environment.
fn service_env(base: &[String], overrides: &[String]) -> Option<Vec<String>> {
    if base.is_empty() && overrides.is_empty() {
        return None;
    }
    let fallback: Vec<String>;
    let base = if base.is_empty() {
        fallback = spec::DEFAULT_ENV.iter().map(|s| String::from(*s)).collect();
        &fallback
    } else {
        base
    };
    Some(spec::compose_env(base, overrides))
}

fn spawn_in_box(
    box_id: u64,
    command: &str,
    args: &[&str],
    env: Option<&[String]>,
) -> Option<SpawnResult> {
    let mut options = sys::SpawnOptions {
        cwd_ptr: "/".as_ptr() as u64,
        cwd_len: 1,
        root_dir_ptr: 0,
        root_dir_len: 0,
        args_ptr: 0,
        args_len: 0,
        stdin_ptr: 0,
        stdin_len: 0,
        box_id,
        env_ptr: 0,
        env_len: 0,
    };
    let args_opt = if args.is_empty() { None } else { Some(args) };
    sys::spawn_ext_env(command, args_opt, env, None, &mut options)
}

/// Set up mounts in a box's namespace from OCI config mount entries.
fn setup_oci_mounts(box_id: u64, mounts: &[OciMount]) {
    for m in mounts {
        match m.mount_type.as_str() {
            "proc" | "tmpfs" => {}
            _ => continue,
        };

        if libakuma::mount_in_ns(box_id, &m.destination, &m.mount_type, None) != 0 {
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
        let (fstype, dest) = match m.as_str() {
            "proc" => ("proc", "/proc"),
            "tmpfs" => ("tmpfs", "/tmp"),
            _ => continue,
        };
        if libakuma::mount_in_ns(box_id, dest, fstype, None) != 0 {
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
                // core_init failed (likely ENOSYS in shared-kernel SMP mode)
                // Fall back to starting as a normal local process on the BSP
                print("[herd] core_init unavailable for ");
                print(name);
                print(" — falling back to local process on BSP\n");
                // Clear the core pinning and continue to normal spawning below
                let config_for_local = ServiceConfig {
                    core: 0, // Force BSP (core 0)
                    ..config.clone()
                };
                let args: Vec<&str> = config_for_local.args.iter().map(|s| s.as_str()).collect();
                let args_opt = if args.is_empty() { None } else { Some(args.as_slice()) };
                let spawn_res = spawn(&config_for_local.command, args_opt);
                
                match spawn_res {
                    Some(SpawnResult { pid, stdout_fd }) => {
                        svc.pid = Some(pid);
                        svc.stdout_fd = Some(nonblocking_stdout(stdout_fd));
                        svc.state = ServiceState::Running;
                        svc.restart_at_ms = None;
                        print("[herd] Started ");
                        print(name);
                        print(" (pid=");
                        print_dec(pid as usize);
                        print(") on BSP fallback\n");
                    }
                    None => {
                        svc.state = ServiceState::Failed;
                        print("[herd] Failed to start ");
                        print(name);
                        print("\n");
                    }
                }
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
        let target_box_id = box_id_for(&config.join_box);
        setup_fs_mounts(target_box_id, &config.mount_fs);
        let args: Vec<&str> = config.args.iter().map(|s| s.as_str()).collect();
        let env = service_env(&[], &config.env);
        spawn_in_box(target_box_id, &config.command, &args, env.as_deref())
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

        let box_id = box_id_for(name);

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
        sys::register_box(box_id, name, &root_dir, 0);
        if config.stack == "rump" {
            sys::set_box_stack_rump(box_id);
        }

        // 2. Set up OCI mounts in the box's namespace
        setup_oci_mounts(box_id, &oci.mounts);

        // 3. Spawn the main process (namespace handles path resolution).
        //    The bundle's `process.env` is the service's base environment — the
        //    OCI runtime spec says it is the container's environment — and the
        //    service's own `env =` lines override it by name.
        let env = service_env(&oci.process_env, &config.env);
        let res = spawn_in_box(box_id, &command, &args, env.as_deref());
        if let Some(ref r) = res {
            sys::register_box(box_id, name, &root_dir, r.pid);
        }
        res
    } else if config.boxed {
        let box_id = box_id_for(name);
        let args: Vec<&str> = config.args.iter().map(|s| s.as_str()).collect();
        sys::register_box(box_id, name, &config.box_root, 0);
        if config.stack == "rump" {
            // Mark the box BEFORE spawning so the kernel knows this box's
            // rump_server should get a sysproxy channel wired onto fd 3 when we
            // spawn it below. herd owns the rump_server lifecycle (one server,
            // no second kernel-spawned one); the kernel only attaches the
            // channel + drives the proxy.
            sys::set_box_stack_rump(box_id);
        }
        setup_fs_mounts(box_id, &config.mount_fs);
        let env = service_env(&[], &config.env);
        let res = spawn_in_box(box_id, &config.command, &args, env.as_deref());
        if let Some(ref r) = res {
            sys::register_box(box_id, name, &config.box_root, r.pid);
        }
        res
    } else {
        let args: Vec<&str> = config.args.iter().map(|s| s.as_str()).collect();
        let args_opt = if args.is_empty() { None } else { Some(args.as_slice()) };
        if config.workdir.is_empty() {
            match service_env(&[], &config.env) {
                Some(env) => {
                    let refs: Vec<&str> = env.iter().map(|s| s.as_str()).collect();
                    spawn_with_env(&config.command, args_opt, None, &refs)
                }
                // No `env =` lines: keep the plain spawn, so an unboxed
                // service's environment is exactly what it was before this
                // feature existed.
                None => spawn(&config.command, args_opt),
            }
        } else {
            // `workdir` is set, so this has to go through `SPAWN_EXT`, which is
            // the only spawn that carries one.
            //
            // A chdir around the plain spawn does **not** work and looks like it
            // should: the child does not inherit our working directory, the
            // kernel gives it the one in `SpawnOptions` (defaulting to `/`).
            // Measured 2026-09-20 with a `pwd` oneshot — it printed `/` with the
            // chdir in place, which is exactly the answer a no-op produces.
            let env = service_env(&[], &config.env);
            let mut options = sys::SpawnOptions {
                cwd_ptr: config.workdir.as_ptr() as u64,
                cwd_len: config.workdir.len(),
                root_dir_ptr: 0,
                root_dir_len: 0,
                args_ptr: 0,
                args_len: 0,
                stdin_ptr: 0,
                stdin_len: 0,
                box_id: 0,
                env_ptr: 0,
                env_len: 0,
            };
            sys::spawn_ext_env(&config.command, args_opt, env.as_deref(), None, &mut options)
        }
    };

    match spawn_res {
        Some(SpawnResult { pid, stdout_fd }) => {
            if let Some(svc) = state.services.get_mut(name) {
                svc.pid = Some(pid);
                svc.stdout_fd = Some(nonblocking_stdout(stdout_fd));
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

/// The answer to a control request: `ok` decides the CLI's exit status, `msg`
/// is what it prints (and what the daemon logs).
struct Reply {
    ok: bool,
    msg: String,
}

impl Reply {
    fn ok(msg: String) -> Self {
        Self { ok: true, msg }
    }

    fn err(msg: String) -> Self {
        Self { ok: false, msg }
    }
}

/// Config-reload removal of a disabled service: kill and reap it, like
/// [`halt_service`], landing on `Stopped` (the entry is removed right after).
fn stop_service(state: &mut HerdState, name: &str) -> Reply {
    kill_service_to(state, name, ServiceState::Stopped)
}

/// Administrative stop (`herd stop <svc>`): kills and reaps the running
/// process like [`stop_service`], but lands on [`ServiceState::Halted`]
/// instead of `Stopped` so `start_stopped_services` does not revive it on the
/// very next pass. See [`ServiceState::Halted`].
fn halt_service(state: &mut HerdState, name: &str) -> Reply {
    kill_service_to(state, name, ServiceState::Halted)
}

/// How [`terminate`] ended.
#[derive(Debug, PartialEq, Eq)]
enum Termination {
    /// Reaped, as `(how, shell code)`; `escalated` if it took SIGKILL.
    Reaped { how: Exit, code: i32, escalated: bool },
    /// The signal was refused (no such process) and there was nothing to reap.
    Gone,
    /// Still not reapable after SIGKILL and both grace periods.
    Stuck,
}

/// SIGTERM, wait up to [`STOP_TERM_GRACE_MS`] for the process to be reapable,
/// then SIGKILL and wait up to [`STOP_KILL_GRACE_MS`] more.
///
/// `signal` is `kill_signal(pid, _)`, `reap` a non-blocking `waitpid` of the
/// same pid, `elapsed_ms` the time since the stop began and `nap` the poll
/// sleep — all effects, so the escalation is host-tested.
///
/// Blocks the supervisor for at most the two grace periods. That is the point:
/// the old stop signalled, forgot the pid and returned, so herd could not say
/// whether the process was gone, and a `herd start` right after it would launch
/// a second copy beside the first.
fn terminate(
    mut signal: impl FnMut(u32) -> i32,
    mut reap: impl FnMut() -> Option<(Exit, i32)>,
    mut elapsed_ms: impl FnMut() -> u64,
    mut nap: impl FnMut(),
) -> Termination {
    if signal(SIGTERM) < 0 {
        // Already dead and waiting to be reaped refuses a signal too.
        return match reap() {
            Some((how, code)) => Termination::Reaped { how, code, escalated: false },
            None => Termination::Gone,
        };
    }
    let mut escalated = false;
    loop {
        if let Some((how, code)) = reap() {
            return Termination::Reaped { how, code, escalated };
        }
        let t = elapsed_ms();
        if !escalated && t >= STOP_TERM_GRACE_MS {
            escalated = true;
            let _ = signal(SIGKILL);
        } else if t >= STOP_TERM_GRACE_MS + STOP_KILL_GRACE_MS {
            return Termination::Stuck;
        }
        nap();
    }
}

fn kill_service_to(state: &mut HerdState, name: &str, target: ServiceState) -> Reply {
    let Some(svc) = state.services.get_mut(name) else {
        return Reply::err(format!("{name}: not enabled"));
    };
    let pid = svc.pid.take();
    let stdout_fd = svc.stdout_fd.take();
    let was_stopping = svc.stopping_pid;
    svc.state = target;
    svc.restart_at_ms = None;

    let Some(pid) = pid else {
        if let Some(fd) = stdout_fd {
            close(fd as i32);
        }
        return Reply::ok(match was_stopping {
            Some(old) => format!("{name}: not running (pid {old} from an earlier stop has still not exited)"),
            None => format!("{name}: not running"),
        });
    };

    let begin = uptime();
    let outcome = terminate(
        // Must carry a real signal: `libakuma::kill` hardcodes sig 0, which the
        // kernel treats as an existence probe and never delivers.
        |sig| kill_signal(pid, sig),
        || {
            waitpid_status(pid).map(|status| {
                (Exit { signaled: status.signaled(), exit_code: status.exit_code() }, status.shell_code())
            })
        },
        || uptime().saturating_sub(begin) / 1000,
        || sleep_ms(STOP_POLL_MS),
    );

    // Keep whatever it wrote on the way out.
    if let Some(fd) = stdout_fd {
        let data = drain_readable(|buf| read_fd(fd as i32, buf));
        close(fd as i32);
        append_to_log(state, name, &data);
    }

    let Some(svc) = state.services.get_mut(name) else {
        return Reply::err(format!("{name}: vanished while stopping"));
    };
    match outcome {
        Termination::Reaped { how, code, escalated } => {
            svc.last_exit_code = Some(code);
            let ended = if how.signaled {
                format!("killed by signal {}", code - 128)
            } else {
                format!("exited with code {code}")
            };
            let forced = if escalated {
                format!(", after ignoring SIGTERM for {} s", STOP_TERM_GRACE_MS / 1000)
            } else {
                String::new()
            };
            Reply::ok(format!("stopped {name} (pid {pid}): {ended}{forced}"))
        }
        Termination::Gone => Reply::ok(format!("stopped {name}: pid {pid} was already gone")),
        Termination::Stuck => {
            svc.stopping_pid = Some(pid);
            Reply::err(format!(
                "{name}: pid {pid} has not exited {} s after SIGKILL; herd reaps it when it does, \
                 and `herd start {name}` refuses until then",
                STOP_KILL_GRACE_MS / 1000
            ))
        }
    }
}

/// `herd start <svc>`: start a service that is not running, clearing a `Halted`
/// or `Exited` state and the restart count.
fn control_start(state: &mut HerdState, name: &str) -> Reply {
    // Enabled since the last 20 s reload: pick it up now rather than make the
    // caller wait for one.
    if !state.services.contains_key(name) {
        reload_config(state);
    }
    let Some(svc) = state.services.get_mut(name) else {
        return Reply::err(format!("{name}: not enabled; run `herd enable {name}` first"));
    };
    if let Some(old) = svc.stopping_pid {
        return Reply::err(format!(
            "{name}: pid {old} from the last stop has not exited yet; not starting a second copy"
        ));
    }
    if svc.state == ServiceState::Running {
        return Reply::ok(match svc.pid {
            Some(pid) => format!("{name}: already running (pid {pid})"),
            None => format!("{name}: already running on core {}", svc.config.core),
        });
    }
    svc.restart_count = 0;
    let config = svc.config.clone();
    start_service(state, name, &config);

    match state.services.get(name).map(|svc| (svc.state, svc.pid)) {
        Some((ServiceState::Running, Some(pid))) => Reply::ok(format!("started {name} (pid {pid})")),
        Some((ServiceState::Running | ServiceState::Completed, None)) => {
            Reply::ok(format!("started {name} on core {}", config.core))
        }
        _ => Reply::err(format!("{name}: failed to start; see the console and `herd log {name}`")),
    }
}

// ============================================================================
// Control Socket
// ============================================================================

/// A `herd start`/`herd stop` request line.
#[derive(Debug, PartialEq, Eq)]
enum Request<'a> {
    Start(&'a str),
    Stop(&'a str),
}

/// `start <svc>` or `stop <svc>`, whitespace-separated, nothing else.
fn parse_request(line: &str) -> Option<Request<'_>> {
    let mut words = line.split_whitespace();
    let verb = words.next()?;
    let name = words.next()?;
    if words.next().is_some() || name.contains('/') {
        return None;
    }
    match verb {
        "start" => Some(Request::Start(name)),
        "stop" => Some(Request::Stop(name)),
        _ => None,
    }
}

/// The wire form of a reply: `ok <msg>\n` / `err <msg>\n`.
fn encode_reply(reply: &Reply) -> String {
    format!("{} {}\n", if reply.ok { "ok" } else { "err" }, reply.msg)
}

/// Split a received reply back into `(ok, msg)`. Anything that is not one of
/// the two tags is reported as a failure, verbatim.
fn decode_reply(wire: &str) -> (bool, &str) {
    let line = wire.trim_end_matches('\n');
    if let Some(msg) = line.strip_prefix("ok ") {
        (true, msg)
    } else if let Some(msg) = line.strip_prefix("err ") {
        (false, msg)
    } else {
        (false, line)
    }
}

/// Bind [`HERD_CONTROL_ADDR`], non-blocking. A listener that cannot be made
/// non-blocking is dropped rather than kept: a blocking `accept` would park the
/// whole supervisor until someone ran `herd start`.
fn open_control_socket() -> Option<TcpListener> {
    let listener = match TcpListener::bind(HERD_CONTROL_ADDR) {
        Ok(l) => l,
        Err(_) => {
            print("[herd] Warning: cannot listen on ");
            print(HERD_CONTROL_ADDR);
            print("; `herd start`/`herd stop` will not reach this daemon\n");
            return None;
        }
    };
    if listener.set_nonblocking(true).is_err() {
        print("[herd] Warning: control socket cannot be made non-blocking; closing it\n");
        return None;
    }
    Some(listener)
}

/// Answer the control connections pending since the last tick, each in full:
/// read its request, act on it, write the reply, close.
fn serve_control(state: &mut HerdState) {
    for _ in 0..MAX_CONTROL_REQUESTS_PER_TICK {
        let Some(listener) = state.control.as_ref() else { return };
        // `WouldBlock` is the idle case; any other error is retried next tick.
        let Ok((stream, _)) = listener.try_accept() else { return };
        let reply = match read_request_line(&stream) {
            Some(line) => handle_request(state, &line),
            None => Reply::err(String::from("no request received")),
        };
        let _ = stream.write_all(encode_reply(&reply).as_bytes());
    }
}

fn handle_request(state: &mut HerdState, line: &str) -> Reply {
    let reply = match parse_request(line) {
        Some(Request::Stop(name)) => {
            print("[herd] Stop requested for ");
            print(name);
            print("\n");
            halt_service(state, name)
        }
        Some(Request::Start(name)) => {
            print("[herd] Start requested for ");
            print(name);
            print("\n");
            control_start(state, name)
        }
        None => Reply::err(format!("bad request {line:?}; expected `start <svc>` or `stop <svc>`")),
    };
    print("[herd] ");
    print(&reply.msg);
    print("\n");
    reply
}

/// The first line a control client sends, waiting at most
/// [`CONTROL_READ_TIMEOUT_MS`] — a client that connects and says nothing must
/// not stall the supervisor.
fn read_request_line(stream: &TcpStream) -> Option<String> {
    let _ = libakuma::set_nonblocking(stream.as_raw_fd(), true);
    let begin = uptime();
    let mut buf = [0u8; 256];
    let mut len = 0;
    while len < buf.len() && !buf[..len].contains(&b'\n') {
        match stream.read(&mut buf[len..]) {
            Ok(0) => break,
            Ok(n) => len += n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if uptime().saturating_sub(begin) / 1000 >= CONTROL_READ_TIMEOUT_MS {
                    return None;
                }
                sleep_ms(5);
            }
            Err(_) => return None,
        }
    }
    let text = core::str::from_utf8(&buf[..len]).ok()?;
    text.lines().next().map(|l| String::from(l.trim()))
}

// ============================================================================
// Output Polling
// ============================================================================

/// Make a freshly spawned service's stdout non-blocking, so draining it can
/// never park the supervisor.
///
/// A `ChildStdout` read with nothing buffered **blocks** until the child writes
/// or exits (`akuma-syscalls-glue`'s read arm) unless the fd is `O_NONBLOCK`.
/// Without this, one quiet service (sshd between connections) froze the whole
/// loop inside `poll_all_stdout`: no restarts, no reloads, and — because each
/// pass drained only 1 KiB — a chatty neighbour's output piling up in its
/// channel. That backlog is what turned the orphan sweep's `wait4` loop into
/// a machine-wide memory leak on 2026-09-24
/// (`docs/archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md` §6 item 6).
fn nonblocking_stdout(fd: u32) -> u32 {
    if libakuma::set_nonblocking(fd as i32, true) < 0 {
        print("[herd] Warning: could not make a service stdout non-blocking\n");
    }
    fd
}

/// Upper bound on reads per service per pass: drain what is buffered, but a
/// service that writes faster than herd reads must not keep it in this loop.
const MAX_READS_PER_PASS: usize = 64;

/// Everything `read` has buffered, until EAGAIN (< 0) or EOF (0), at most
/// [`MAX_READS_PER_PASS`] reads. The fd is non-blocking, so an empty channel
/// costs one syscall rather than parking herd. `read` is `read_fd` in herd and
/// a fake in the tests.
fn drain_readable(mut read: impl FnMut(&mut [u8]) -> isize) -> Vec<u8> {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    for _ in 0..MAX_READS_PER_PASS {
        let n = read(&mut buf);
        if n <= 0 {
            break;
        }
        data.extend_from_slice(&buf[..n as usize]);
    }
    data
}

fn poll_all_stdout(state: &mut HerdState) {
    let mut outputs: Vec<(String, Vec<u8>)> = Vec::new();

    for (name, svc) in state.services.iter() {
        if let Some(fd) = svc.stdout_fd {
            let data = drain_readable(|buf| read_fd(fd as i32, buf));
            if !data.is_empty() {
                outputs.push((name.clone(), data));
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

/// Most `wait4(-1)` reaps one orphan sweep takes before leaving the rest to the
/// next tick.
const MAX_ORPHAN_REAPS: usize = 256;

/// What one [`sweep_reaped`] pass found.
#[derive(Debug, PartialEq, Eq)]
struct Sweep {
    /// Reaped pids that were not a running service's.
    orphans: usize,
    /// The pid that came back a second time and ended the sweep, if one did.
    repeated: Option<u32>,
}

/// Drain `next` (`wait4(-1, WNOHANG)`, as `(pid, how, shell code)`) into
/// `exited` for pids `running_service` names, counting the rest as orphans.
///
/// Terminates whatever `next` does: at its `None`, at the first pid it returns
/// twice, or after [`MAX_ORPHAN_REAPS`]. And a service already in `exited` is
/// never pushed again — the leak this replaced was exactly one dead service
/// returned ~900 k times a second and pushed every time.
fn sweep_reaped(
    exited: &mut Vec<(String, Exit, i32)>,
    running_service: impl Fn(u32) -> Option<String>,
    mut next: impl FnMut() -> Option<(u32, Exit, i32)>,
) -> Sweep {
    let mut sweep = Sweep { orphans: 0, repeated: None };
    let mut seen: Vec<u32> = Vec::new();
    while seen.len() < MAX_ORPHAN_REAPS {
        let Some((pid, how, code)) = next() else { break };
        if seen.contains(&pid) {
            sweep.repeated = Some(pid);
            break;
        }
        seen.push(pid);
        match running_service(pid) {
            Some(name) if exited.iter().any(|(n, _, _)| *n == name) => {}
            Some(name) => exited.push((name, how, code)),
            None => sweep.orphans += 1,
        }
    }
    sweep
}

fn check_process_exits(state: &mut HerdState, now_ms: u64) {
    // Reap with waitpid_status, not waitpid: the latter returns WEXITSTATUS only,
    // so a service killed by a signal reports 0 and is indistinguishable from a
    // clean success. See docs/SIGNAL_EXIT_HANDLING.md.
    let mut exited: Vec<(String, Exit, i32)> = Vec::new();

    for (name, svc) in state.services.iter() {
        if svc.state == ServiceState::Running {
            if let Some(pid) = svc.pid {
                if let Some(status) = waitpid_status(pid) {
                    let how = Exit { signaled: status.signaled(), exit_code: status.exit_code() };
                    // 128+signal for a signal death, the $? convention — so a
                    // SIGSEGV is reported as 139 rather than as a success.
                    exited.push((name.clone(), how, status.shell_code()));
                }
            }
        }
    }

    // Orphans. herd is pid 1, so every process whose parent died is
    // reparented to it — a service's worker children after `herd disable`
    // killed the service, a `nohup … &` from an ssh session that ended, a
    // pipeline whose shell left first. The targeted waits above never ask
    // about those, so each one sat in `Z` forever holding a task slot: 11 of
    // them on the bare-metal box after one evening of probes
    // (`docs/archive/AKUMA_AMD64_TRAP_ENTRY_DIRECTION_FLAG.md` § "Found while
    // checking process/socket cleanup"), and every stale row is one more
    // `[TRAMP-MISMATCH]` line. init reaps; the kernel is right not to do it
    // for us. `wait4(-1, WNOHANG)` until it has nothing left.
    //
    // A *service* pid can come back from this sweep too, if it exited between
    // its targeted wait and here. Route it through the same exit handling: a
    // pid reaped once can never be waited for again, so dropping it would leave
    // the service marked Running with a pid that no longer exists.
    //
    // **Bounded, and never trusting `wait4` to make progress.** A reaped pid
    // must never come back, but on 2026-09-24 one did (a kernel bug, since
    // fixed in `reap_child_channel`): the same dead service ~900 k times a
    // second, each pass pushing onto `exited`, until the machine had no
    // physical memory left (`docs/archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md`
    // §6 item 6). So a pid seen twice ends the sweep, a service is recorded
    // at most once, and the sweep stops after `MAX_ORPHAN_REAPS` regardless —
    // anything left is picked up on the next tick.
    let services = &state.services;
    let mut reaped: Vec<u32> = Vec::new();
    let sweep = sweep_reaped(
        &mut exited,
        |pid| {
            services.iter()
                .find(|(_, svc)| svc.state == ServiceState::Running && svc.pid == Some(pid))
                .map(|(name, _)| name.clone())
        },
        || {
            wait_any().map(|status| {
                reaped.push(status.pid);
                let how = Exit { signaled: status.signaled(), exit_code: status.exit_code() };
                (status.pid, how, status.shell_code())
            })
        },
    );
    // A pid `herd stop` gave up on (`stopping_pid`) comes back here as an
    // "orphan" once it finally exits; that is what lets `herd start` run again.
    for (name, svc) in state.services.iter_mut() {
        if let Some(pid) = svc.stopping_pid.filter(|p| reaped.contains(p)) {
            svc.stopping_pid = None;
            print("[herd] reaped ");
            print(name);
            print("'s stopped pid ");
            print_dec(pid as usize);
            print("\n");
        }
    }
    if let Some(pid) = sweep.repeated {
        print("[herd] Warning: wait4 returned pid ");
        print_dec(pid as usize);
        print(" twice; ending the orphan sweep\n");
    }
    if sweep.orphans > 0 {
        print("[herd] reaped ");
        print_dec(sweep.orphans);
        print(" orphaned process(es)\n");
    }

    for (name, how, reported_code) in exited {
        print("[herd] Service ");
        print(&name);
        print(if how.signaled { " killed by signal, code " } else { " exited with code " });
        print_dec(reported_code as usize);
        print("\n");

        if let Some(svc) = state.services.get_mut(&name) {
            // Close stdout fd
            if let Some(fd) = svc.stdout_fd {
                close(fd as i32);
            }
            svc.pid = None;
            svc.stdout_fd = None;
            svc.last_exit_code = Some(reported_code);

            let policy = Policy {
                oneshot: svc.config.oneshot,
                restart: svc.config.restart,
                max_retries: svc.config.max_retries,
            };

            let outcome = classify(policy, svc.restart_count, how);
            svc.state = ServiceState::after(outcome);
            match outcome {
                // A oneshot service ran its single time: move it to the terminal
                // Completed state (never restarted — start_stopped_services only
                // revives Stopped), regardless of exit code. A reboot runs it again.
                Outcome::Completed => {
                    svc.restart_count = 0;
                    print("[herd] Oneshot service ");
                    print(&name);
                    print(" completed\n");
                }
                Outcome::Restart => {
                    svc.restart_count += 1;
                    svc.restart_at_ms = Some(now_ms + svc.config.restart_delay_ms);
                    print("[herd] Scheduling restart for ");
                    print(&name);
                    print("\n");
                }
                Outcome::Failed => {
                    print("[herd] Service ");
                    print(&name);
                    print(" failed after max retries\n");
                }
                Outcome::Stopped => {
                    svc.restart_count = 0;
                    print("[herd] Service ");
                    print(&name);
                    print(" stays down (exit policy); `herd start ");
                    print(&name);
                    print("` to run it again\n");
                }
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
    print("  start <svc>    Start an enabled service now (no reload wait)\n");
    print("  stop <svc>     Stop and reap a running service now, without disabling it\n");
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

/// Start an enabled service now, and wait for the daemon to say it did.
fn cmd_start(name: &str) -> bool {
    send_control("start", name)
}

/// Stop a running service now: the daemon sends SIGTERM (SIGKILL after
/// [`STOP_TERM_GRACE_MS`]), reaps it, and only then answers. The service stays
/// enabled, so a reboot starts it again; `herd disable` also removes it from
/// `/etc/herd/enabled/`.
fn cmd_stop(name: &str) -> bool {
    send_control("stop", name)
}

/// Send one request to the running daemon on [`HERD_CONTROL_ADDR`] and print
/// its reply. Returns whether the daemon reported success.
fn send_control(verb: &str, name: &str) -> bool {
    let stream = match TcpStream::connect(HERD_CONTROL_ADDR) {
        Ok(s) => s,
        Err(_) => {
            print("Error: cannot reach the herd daemon on ");
            print(HERD_CONTROL_ADDR);
            print(" (is it running?)\n");
            return false;
        }
    };
    let request = format!("{verb} {name}\n");
    if stream.write_all(request.as_bytes()).is_err() {
        print("Error: failed to send the request to the herd daemon\n");
        return false;
    }
    // A stop answers only once the process is reaped, so this can take up to
    // both grace periods.
    let mut wire: Vec<u8> = Vec::new();
    let mut buf = [0u8; 512];
    while let Ok(n) = stream.read(&mut buf) {
        if n == 0 {
            break;
        }
        wire.extend_from_slice(&buf[..n]);
    }
    let text = core::str::from_utf8(&wire).unwrap_or("");
    if text.is_empty() {
        print("Error: the herd daemon closed the connection without a reply\n");
        return false;
    }
    let (ok, msg) = decode_reply(text);
    if !ok {
        print("Error: ");
    }
    print(msg);
    print("\n");
    ok
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

// ============================================================================
// Tests — `cargo test --bin herd --target <host>` (see Cargo.toml). Pure logic
// only: every libakuma wrapper is a raw Akuma syscall, which a host test must
// never reach.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn died(code: i32) -> Exit {
        Exit::code(code)
    }

    #[test]
    fn sweep_ends_when_wait4_returns_the_same_pid_forever() {
        // The 2026-09-24 leak: `wait4(-1)` handed back one dead, still-buffered
        // service on every call and the old loop never stopped.
        let mut exited = Vec::new();
        let mut calls = 0;
        let sweep = sweep_reaped(
            &mut exited,
            |pid| (pid == 20).then(|| String::from("kot")),
            || {
                calls += 1;
                Some((20, died(134), 134))
            },
        );
        assert_eq!(sweep.repeated, Some(20));
        assert_eq!(calls, 2, "the second sighting ends it");
        assert_eq!(exited.len(), 1, "and the service is recorded once");
    }

    #[test]
    fn sweep_never_records_a_service_the_targeted_wait_already_did() {
        let mut exited = vec![(String::from("kot"), died(1), 1)];
        let mut once = Some((20, died(1), 1));
        let sweep = sweep_reaped(&mut exited, |_| Some(String::from("kot")), || once.take());
        assert_eq!(exited.len(), 1);
        assert_eq!(sweep, Sweep { orphans: 0, repeated: None });
    }

    #[test]
    fn sweep_counts_orphans_and_stops_at_the_cap() {
        let mut exited = Vec::new();
        let mut next_pid = 100u32;
        let sweep = sweep_reaped(
            &mut exited,
            |_| None,
            || {
                next_pid += 1;
                Some((next_pid, died(0), 0))
            },
        );
        assert_eq!(sweep.orphans, MAX_ORPHAN_REAPS, "an endless supply is cut off");
        assert!(exited.is_empty());
    }

    #[test]
    fn sweep_stops_at_echild() {
        let mut exited = Vec::new();
        let mut queue = vec![(7, died(0), 0), (8, died(2), 2)];
        let sweep = sweep_reaped(
            &mut exited,
            |pid| (pid == 8).then(|| String::from("svc")),
            || queue.pop(),
        );
        assert_eq!(sweep, Sweep { orphans: 1, repeated: None });
        assert_eq!(exited, vec![(String::from("svc"), died(2), 2)]);
    }

    #[test]
    fn drain_reads_until_eagain() {
        let mut chunks = vec![-11isize, 3, 5]; // popped from the back: 5, 3, EAGAIN
        let mut reads = 0;
        let data = drain_readable(|buf| {
            reads += 1;
            let n = chunks.pop().unwrap_or(-11);
            if n > 0 {
                buf[..n as usize].fill(b'x');
            }
            n
        });
        assert_eq!(data.len(), 8);
        assert_eq!(reads, 3, "one read past the data, to see EAGAIN");
    }

    #[test]
    fn drain_stops_at_eof() {
        let mut first = true;
        let data = drain_readable(|buf| {
            if core::mem::take(&mut first) {
                buf[0] = b'z';
                1
            } else {
                0
            }
        });
        assert_eq!(data, b"z");
    }

    #[test]
    fn drain_is_bounded_for_a_writer_faster_than_herd() {
        let mut reads = 0;
        let data = drain_readable(|buf| {
            reads += 1;
            buf.len() as isize
        });
        assert_eq!(reads, MAX_READS_PER_PASS);
        assert_eq!(data.len(), MAX_READS_PER_PASS * 4096);
    }

    #[test]
    fn no_exit_outcome_is_restarted_by_the_start_pass() {
        // `restart = false` and clean exits used to land in `Stopped` — "not
        // started yet" — and were launched again on the very next pass.
        for outcome in [Outcome::Completed, Outcome::Restart, Outcome::Failed, Outcome::Stopped] {
            assert!(
                !ServiceState::after(outcome).started_by_start_pass(),
                "{outcome:?} must not be revived by start_stopped_services"
            );
        }
        assert_eq!(ServiceState::after(Outcome::Stopped), ServiceState::Exited);
        assert!(ServiceState::Stopped.started_by_start_pass(), "a fresh service still starts");
    }

    /// Drive [`terminate`] against a process that becomes reapable at
    /// `reapable_at` ms (never, if `None`), with `nap` advancing a fake clock.
    fn run_terminate(reapable_at: Option<u64>, signal_ok: bool) -> (Termination, Vec<u32>) {
        use core::cell::Cell;
        let clock = Cell::new(0u64);
        let mut sent = Vec::new();
        let t = terminate(
            |sig| {
                sent.push(sig);
                if signal_ok { 0 } else { -3 }
            },
            || reapable_at.filter(|at| clock.get() >= *at).map(|_| (Exit::signal(), 143)),
            || clock.get(),
            || clock.set(clock.get() + STOP_POLL_MS),
        );
        (t, sent)
    }

    #[test]
    fn stop_reaps_a_process_that_honours_sigterm() {
        let (t, sent) = run_terminate(Some(100), true);
        assert_eq!(t, Termination::Reaped { how: Exit::signal(), code: 143, escalated: false });
        assert_eq!(sent, vec![SIGTERM]);
    }

    #[test]
    fn stop_escalates_to_sigkill_after_the_grace_period() {
        let (t, sent) = run_terminate(Some(STOP_TERM_GRACE_MS + 60), true);
        assert!(matches!(t, Termination::Reaped { escalated: true, .. }));
        assert_eq!(sent, vec![SIGTERM, SIGKILL], "SIGKILL exactly once");
    }

    #[test]
    fn stop_gives_up_on_a_process_that_never_becomes_reapable() {
        // amd64's SIGTERM'd multi-threaded kot, 2026-09-24.
        let (t, sent) = run_terminate(None, true);
        assert_eq!(t, Termination::Stuck);
        assert_eq!(sent, vec![SIGTERM, SIGKILL]);
    }

    #[test]
    fn stop_of_a_vanished_pid_neither_waits_nor_escalates() {
        let (t, sent) = run_terminate(None, false);
        assert_eq!(t, Termination::Gone);
        assert_eq!(sent, vec![SIGTERM]);
        // ...but one that already exited is still reaped.
        let (t, _) = run_terminate(Some(0), false);
        assert!(matches!(t, Termination::Reaped { escalated: false, .. }));
    }

    #[test]
    fn requests_parse() {
        assert_eq!(parse_request("stop kot"), Some(Request::Stop("kot")));
        assert_eq!(parse_request("  start   sshd \r"), Some(Request::Start("sshd")));
        assert_eq!(parse_request("stop"), None);
        assert_eq!(parse_request("stop kot now"), None);
        assert_eq!(parse_request("restart kot"), None);
        assert_eq!(parse_request("stop ../kot"), None);
        assert_eq!(parse_request(""), None);
    }

    #[test]
    fn replies_round_trip() {
        let ok = encode_reply(&Reply::ok(String::from("stopped kot (pid 20): exited with code 0")));
        assert_eq!(decode_reply(&ok), (true, "stopped kot (pid 20): exited with code 0"));
        let err = encode_reply(&Reply::err(String::from("kot: not enabled")));
        assert_eq!(decode_reply(&err), (false, "kot: not enabled"));
        assert_eq!(decode_reply("garbage"), (false, "garbage"), "an untagged reply is a failure");
    }

    #[test]
    fn restart_false_exit_stays_down() {
        let policy = Policy { oneshot: false, restart: false, max_retries: 0 };
        let state = ServiceState::after(classify(policy, 0, died(7)));
        assert_eq!(state, ServiceState::Exited);
    }
}
