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
    open_flags, seek_mode, spawn, spawn_console, spawn_with_env, kill_signal, waitpid_status, wait_any, read_dir,
    uptime, sleep_ms, mkdir_p, SpawnResult, SIGKILL, SIGTERM,
};
use libakuma::net::{ErrorKind, TcpListener, TcpStream};

use boxlib::json;
use boxlib::spec::{self, box_id_for};
use boxlib::sys;

use herd::exit::{classify, classify_spawn_failure, next_streak, restart_delay_ms, Exit, Outcome, Policy};
use herd::vm::{self, VmConfig};

// ============================================================================
// Constants
// ============================================================================

/// Config reload interval in milliseconds (20 seconds)
const CONFIG_RELOAD_INTERVAL_MS: u64 = 20_000;

/// Supervisor poll interval in milliseconds
const POLL_INTERVAL_MS: u64 = 100;

/// Maximum log file size before rotation (32KB)
const MAX_LOG_SIZE: usize = 32 * 1024;

/// The same for a `vm = true` service: a guest's boot console alone is ~30 KB, and its log is the
/// only record of it, so rotating at 32 KB threw the boot away.
const VM_MAX_LOG_SIZE: usize = 256 * 1024;

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
/// AF_UNIX (`crates/akuma-kernel-amd64/src/sock.rs::sys_socket` answers `EAFNOSUPPORT`) and herd
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

/// How long a control connection gets to send its request line. The CLI
/// sends its line straight after connecting; this only bounds a client that
/// connects and says nothing, during which the supervisor does nothing else.
const CONTROL_READ_TIMEOUT_MS: u64 = 250;

/// Time `serve_control` may spend on silent or slow clients in one tick before
/// it leaves the rest for the next one. A `stop` that is waiting for a reap is
/// bounded by the stop grace periods instead, and is not cut short.
const CONTROL_TICK_BUDGET_MS: u64 = 500;

/// Where the daemon that owns [`HERD_CONTROL_ADDR`] writes its control token
/// (hex, mode 0600), which `start` and `stop` requests must carry.
///
/// The socket is loopback TCP, so anything that can open a loopback
/// connection can reach it: every local user, and every boxed service that
/// shares the host's network stack. Until 2026-09-25 the kernel also bound
/// `127.0.0.1` as any-address, which put `stop <svc>` on the LAN
/// (`docs/archive/AKUMA_NET_LOOPBACK_BIND_EXPOSED.md`). The token limits
/// `start`/`stop` to callers who can read this file. `status` is a read and
/// stays open.
const CONTROL_TOKEN_PATH: &str = "/var/run/herd/control.token";

/// Bytes of randomness in the control token (hex-encoded on the wire).
const CONTROL_TOKEN_BYTES: usize = 16;

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
    
    // Create /var/run/herd (the specs `vm-run` reads)
    if !mkdir_p(vm::SPEC_DIR) {
        print("[herd] Warning: Failed to create ");
        print(vm::SPEC_DIR);
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
    /// Stretch `restart_delay_ms` for a service that keeps failing quickly
    /// (`herd::exit::restart_delay_ms`). Default **false**: a service that must
    /// come back promptly (wifi) would otherwise wait up to a minute between tries.
    backoff: bool,
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
    /// Give the service **the console** instead of a pipe: it reads the keyboard
    /// and writes the screen through the console's own line discipline. One
    /// service per machine can meaningfully have it — it is what puts a login
    /// shell on the television of a box with no ssh. Needs the amd64 kernel's
    /// `SPAWN_FLAG_CONSOLE` and herd running as init; elsewhere the spawn is
    /// refused (`Failed to start`) or the flag ignored, never silently wrong.
    /// Takes precedence over `workdir`, which the plain-spawn syscall cannot
    /// carry, and does not combine with a box.
    console: bool,
    /// Multikernel core pin (docs/MULTIKERNEL.md §10, CORE_AWARE_SCHEDULING.md). 0 =
    /// unpinned / BSP (current behavior: spawn locally on core 0). Non-zero = run this
    /// service on that secondary core's kernel: herd hands the kernel the command path in
    /// the `core_init` activation message and that core spawns it LOCALLY (no cross-core
    /// spawn). A pinned service has no local pid — it lives on its core, its output drains
    /// via that core's console ring, and its exit is reaped by that core's kernel.
    /// Mutually exclusive with boxes (see `is_boxed`).
    core: u32,
    /// `cpus = 4-7`: the host cores the service (and everything it spawns) may run on, 0 = any.
    /// herd sets its own affinity to the mask around the spawn, so the child is born with it
    /// (affinity is inherited) and never runs elsewhere; a core the kernel does not have is
    /// refused by the kernel and the service starts unrestricted, with a warning.
    cpus: u64,
    /// `vm = true`: this service is a KVM guest (`docs/reference/subsystems/kvm.md`). The
    /// supervised process is `herd vm-run <spec file>`; `command` is [`vm::HELPER`].
    vm: Option<akuma_vm_spec::Spec>,
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
            backoff: false,
            join_box: String::new(),
            mount_fs: Vec::new(),
            start_delay_ms: 0,
            oneshot: false,
            workdir: String::new(),
            console: false,
            core: 0,
            cpus: 0,
            vm: None,
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
    /// Quick failures in a row (`herd::exit::next_streak`): what stretches the
    /// restart delay for a service that cannot stay up.
    fail_streak: u32,
    /// When the running instance started (uptime ms), to tell a quick failure
    /// from one after a healthy run.
    started_at_ms: Option<u64>,
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
    /// The config file text `config` was parsed from. A reload re-parses only
    /// a file whose text changed, so a warning about a bad line is printed
    /// once per edit, not every 20 s.
    config_text: String,
}

impl SupervisedProcess {
    fn new(config: ServiceConfig, config_text: String, log_size: usize) -> Self {
        Self {
            config,
            config_text,
            pid: None,
            stdout_fd: None,
            state: ServiceState::Stopped,
            restart_count: 0,
            fail_streak: 0,
            started_at_ms: None,
            last_exit_code: None,
            restart_at_ms: None,
            start_at_ms: None,
            log_size,
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
    /// The token `start`/`stop` must carry ([`CONTROL_TOKEN_PATH`]); `None`
    /// if it could not be written, in which case they are accepted without.
    control_token: Option<String>,
    /// Config files that did not parse, by service name, with the text that
    /// failed: the error is printed once per edit rather than every reload.
    rejected_configs: BTreeMap<String, String>,
}

impl HerdState {
    fn new() -> Self {
        Self {
            services: BTreeMap::new(),
            last_config_reload_ms: 0,
            pinned_cores: BTreeMap::new(),
            control: None,
            control_token: None,
            rejected_configs: BTreeMap::new(),
        }
    }
}

// ============================================================================
// Entry Point
// ============================================================================

#[cfg_attr(not(test), no_mangle)]
pub extern "C" fn main() {
    // The helper a VM service runs: no supervisor, no directories.
    if libakuma::arg(1) == Some("vm-run") {
        match libakuma::arg(2) {
            Some(path) => vm_run(path),
            None => {
                print("Usage: herd vm-run <spec file>\n");
                exit(1);
            }
        }
    }

    // Check for command-line arguments
    let argc = libakuma::argc();
    let subcommand = if argc > 1 { libakuma::arg(1).unwrap_or("") } else { "daemon" };
    let service_name = libakuma::arg(2);

    // Only the commands that create something need the directory tree; a
    // `herd status` or `herd log` must not mkdir (or warn) as a side effect.
    if matches!(subcommand, "daemon" | "run" | "foreground" | "fg" | "add" | "enable")
        || subcommand.starts_with("--")
    {
        ensure_directories();
    }

    // A subcommand that takes a service name: the name, checked, or exit 1.
    let named = |usage: &str| -> &str {
        let Some(name) = service_name else {
            print("Usage: herd ");
            print(usage);
            print(" <service>\n");
            exit(1);
        };
        if !valid_service_name(name) {
            print("Error: invalid service name '");
            print(name);
            print("' (no '/', whitespace or leading '.')\n");
            exit(1);
        }
        name
    };
    let done = |ok: bool| -> ! { exit(if ok { 0 } else { 1 }) };

    match subcommand {
        // Run as daemon in foreground (fall through to supervisor loop).
        // `--enabled-dir`/`--service` as the first argument mean the same.
        "daemon" | "run" | "foreground" | "fg" => {}
        s if s.starts_with("--") => {}
        "status" => {
            cmd_status();
            exit(0);
        }
        "add" => done(cmd_add(named("add"))),
        "config" => done(cmd_config(named("config"))),
        "enable" => done(cmd_enable(named("enable"))),
        "disable" => done(cmd_disable(named("disable"))),
        "reload" => done(send_control("reload", "")),
        "start" => done(cmd_start(named("start"))),
        "stop" => done(cmd_stop(named("stop"))),
        "log" => done(cmd_log(named("log"))),
        "version" | "--version" | "-V" => {
            print("herd ");
            print(libakuma::GIT_REV);
            print("\n");
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

    // Daemon mode - run supervisor loop
    print("[herd] Userspace supervisor starting (rev ");
    print(libakuma::GIT_REV);
    print(")...\n");

    let mut state = HerdState::new();

    state.control = open_control_socket();
    // Only the daemon that owns the socket writes the token: a second,
    // core-pinned herd (whose bind fails) must not replace the first's.
    if state.control.is_some() {
        state.control_token = write_control_token();
    }

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

/// A config problem worth a console line. Not under the host test harness,
/// where `print` is an Akuma syscall.
fn config_warning(what: &str, key: &str, value: &str) {
    if !cfg!(test) {
        print("[herd] Warning: ");
        print(what);
        print(" `");
        print(key);
        print(" = ");
        print(value);
        print("`\n");
    }
}

/// `true`/`yes`/`on`/`1` or `false`/`no`/`off`/`0`; anything else is `None`.
///
/// One spelling set for every boolean key. They used to differ per key:
/// `backoff` and `console` took `yes`, `boxed` and `oneshot` did not, and
/// `restart` read anything but `false`/`0`/`no` as true — so
/// `oneshot = yes` silently made a restarting service.
fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// `value` without a trailing ` # comment`.
///
/// The README's own example writes `restart_delay = 1000     # ms ...`, and
/// that value did not parse as a number: it fell back to the default, which
/// happened to be 1000 too. `restart_delay = 5000  # five seconds` silently
/// became one second. A `#` counts as a comment only after whitespace, so a
/// path or argument containing `#` is kept; `env` values are never cut.
fn strip_inline_comment(value: &str) -> &str {
    let b = value.as_bytes();
    for i in 1..b.len() {
        if b[i] == b'#' && (b[i - 1] == b' ' || b[i - 1] == b'\t') {
            return value[..i].trim_end();
        }
    }
    value
}

fn parse_service_config(content: &str) -> Option<ServiceConfig> {
    let mut config = ServiceConfig::default();
    let mut vmc = VmConfig::default();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = if key == "env" { value.trim() } else { strip_inline_comment(value.trim()) };
            // A value that does not parse keeps the default, and says so.
            let bool_or = |default: bool| {
                parse_bool(value).unwrap_or_else(|| {
                    config_warning("not a boolean, keeping the default:", key, value);
                    default
                })
            };

            // `vm`, `vm_*`: gathered whole and checked once, below.
            if vmc.apply(key, value) {
                continue;
            }

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
                    config.restart_delay_ms = value.parse::<u64>().unwrap_or_else(|_| {
                        config_warning("not a number of ms, keeping the default:", key, value);
                        DEFAULT_RESTART_DELAY_MS
                    });
                }
                "max_retries" => {
                    config.max_retries = value.parse::<u32>().unwrap_or_else(|_| {
                        config_warning("not a number, keeping the default:", key, value);
                        DEFAULT_MAX_RETRIES
                    });
                }
                "boxed" => {
                    config.boxed = bool_or(config.boxed);
                }
                "box_root" => {
                    config.box_root = String::from(value);
                }
                "bundle" => {
                    config.bundle = String::from(value);
                    config.boxed = true; // bundles are always boxed
                }
                "stack" => config.stack = String::from(value),
                "restart" => config.restart = bool_or(config.restart),
                "backoff" => config.backoff = bool_or(config.backoff),
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
                    config.start_delay_ms = value.parse::<u64>().unwrap_or_else(|_| {
                        config_warning("not a number of ms, keeping the default:", key, value);
                        0
                    });
                }
                "oneshot" => {
                    config.oneshot = bool_or(config.oneshot);
                }
                // Both spellings, because both are the obvious one to reach for.
                "workdir" | "working_dir" => config.workdir = String::from(value),
                "console" => config.console = bool_or(config.console),
                // Repeatable: each line adds one variable. The `split_once('=')`
                // above takes the FIRST `=`, so the value keeps any further ones
                // (`env = DSN=host=db port=5432` is one entry, spaces and all).
                "env" => {
                    if !value.is_empty() {
                        config.env.push(String::from(value));
                    }
                }
                "cpus" => match akuma_vm_spec::cores::parse(value) {
                    Some(mask) => config.cpus = mask,
                    None if value.is_empty() => {}
                    None => {
                        if !cfg!(test) {
                            print("[herd] Warning: bad cpus = value ignored: ");
                            print(value);
                            print("\n");
                        }
                    }
                },
                "core" => {
                    config.core = value.parse::<u32>().unwrap_or_else(|_| {
                        config_warning("not a core number, ignored:", key, value);
                        0
                    });
                }
                // A misspelt key (`restart_dealy`) used to be skipped without
                // a word, leaving the default in force. Still skipped - a newer
                // config on an older herd must load - but now said.
                _ => config_warning("unknown key ignored:", key, value),
            }
        }
    }

    match vmc.finish(read_file_string) {
        Ok(Some(spec)) => {
            config.vm = Some(spec);
            config.command = String::from(vm::HELPER);
        }
        Ok(None) => {}
        Err(e) => {
            // (`print` is an Akuma syscall: not under the host test harness.)
            if !cfg!(test) {
                print("[herd] Warning: vm service not loaded: ");
                print(&e.describe());
                print("\n");
            }
            return None;
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
fn explicit_service_files() -> Option<Vec<(String, Option<String>)>> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut saw_service_flag = false;
    let mut i = 1;
    loop {
        match libakuma::arg(i) {
            Some("--service") => {
                saw_service_flag = true;
                if let Some(path) = libakuma::arg(i + 1) {
                    let base = path.rsplit('/').next().unwrap_or(path);
                    let name = base.trim_end_matches(".conf");
                    let content = read_file_string(path);
                    if content.is_none() {
                        print("[herd] Warning: cannot read --service ");
                        print(path);
                        print("\n");
                    }
                    out.push((String::from(name), content));
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
    //
    // `None` content is a file that is listed but could not be read this time. Its
    // service is kept as it is: a transient read failure used to drop the entry, and
    // the removal pass below then stopped a running service nobody had disabled.
    let entries: Vec<(String, Option<String>)> = match explicit_service_files() {
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
            let mut v: Vec<(String, Option<String>)> = Vec::new();
            for entry in dir {
                if !entry.name.ends_with(".conf") {
                    continue;
                }
                let name = String::from(entry.name.trim_end_matches(".conf"));
                let path = format!("{}/{}", ed, entry.name);
                let content = read_file_string(&path);
                if content.is_none() {
                    print("[herd] Warning: cannot read ");
                    print(&path);
                    print("; keeping the service as it was\n");
                }
                v.push((name, content));
            }
            v
        }
    };

    for (service_name, content) in &entries {
        let Some(content) = content else { continue };
        if state.services.get(service_name).is_some_and(|svc| svc.config_text == *content)
            || state.rejected_configs.get(service_name) == Some(content)
        {
            continue; // unchanged since the last reload
        }

        let Some(config) = parse_service_config(content) else {
            print("[herd] Error parsing ");
            print(service_name);
            if state.services.contains_key(service_name) {
                print("; keeping the previous config");
            }
            print("\n");
            state.rejected_configs.insert(service_name.clone(), content.clone());
            continue;
        };
        state.rejected_configs.remove(service_name);

        if let Some(svc) = state.services.get_mut(service_name) {
            print("[herd] Config changed: ");
            print(service_name);
            if svc.pid.is_some() {
                print(" (takes effect at its next start)");
            }
            print("\n");
            svc.config = config;
            svc.config_text = content.clone();
        } else {
            let svc = SupervisedProcess::new(config, content.clone(), existing_log_size(service_name));
            state.services.insert(service_name.clone(), svc);
        }
    }

    // Remove services that are no longer enabled
    let to_remove: Vec<String> = state.services.keys()
        .filter(|name| !entries.iter().any(|(n, _)| n == *name))
        .cloned()
        .collect();
    state.rejected_configs.retain(|name, _| entries.iter().any(|(n, _)| n == name));

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

/// The size of a service's existing log, so rotation counts what an earlier
/// herd (the previous boot, or a restarted daemon) already wrote. It started
/// at 0, so each herd start let a log grow another [`MAX_LOG_SIZE`].
fn existing_log_size(service_name: &str) -> usize {
    let path = format!("{}/{}.log", HERD_LOG_DIR, service_name);
    libakuma::fstatat(-100, &path, 0).map_or(0, |st| usize::try_from(st.st_size).unwrap_or(0))
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

/// `cwd` is a path inside the box's namespace. It was hard-coded to `/`, so
/// `workdir` was silently ignored for boxed and `join_box` services, and so was
/// an OCI bundle's `process.cwd`.
fn spawn_in_box(
    box_id: u64,
    command: &str,
    args: &[&str],
    env: Option<&[String]>,
    cwd: &str,
) -> Option<SpawnResult> {
    let cwd = if cwd.is_empty() { "/" } else { cwd };
    let mut options = sys::SpawnOptions {
        cwd_ptr: cwd.as_ptr() as u64,
        cwd_len: cwd.len(),
        root_dir_ptr: 0,
        root_dir_len: 0,
        args_ptr: 0,
        args_len: 0,
        stdin_ptr: 0,
        stdin_len: 0,
        box_id,
        env_ptr: 0,
        env_len: 0,
        ..Default::default()
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

/// Start a `vm = true` service: write its spec where `vm-run` reads it and spawn
/// `herd vm-run <file>`. That process **is** the service as far as herd is concerned.
fn spawn_vm_helper(name: &str, spec: &akuma_vm_spec::Spec) -> Option<SpawnResult> {
    let mut wire = [0u8; akuma_vm_spec::spec::MAX_WIRE];
    let n = spec.encode(&mut wire)?;
    let path = vm::spec_path(name);
    if !write_file(&path, &wire[..n]) {
        print("[herd] Error: cannot write ");
        print(&path);
        print("\n");
        return None;
    }
    spawn(vm::HELPER, Some(&["vm-run", path.as_str()]))
}

/// `herd vm-run <spec file>`: the process a VM service is. Creates the VM through `/dev/kvm0`,
/// copies the guest's console to stdout (herd logs it to `/var/log/herd/<service>.log`), and
/// exits when the guest does. The kernel ties the VM's life to this process: if it is killed
/// (`herd stop`, a crash, `kill`), the guest is stopped, and herd's restart policy decides what
/// happens next. Never returns.
fn vm_run(spec_path: &str) -> ! {
    /// `-EBUSY` / `-EAGAIN`.
    const EBUSY: isize = -16;
    const EAGAIN: isize = -11;
    /// How often a `status` line is asked for, so the log carries the kernel's view (µs).
    const STATUS_EVERY_US: u64 = 60_000_000;

    let Some(spec) = read_file_bytes(spec_path) else {
        print("[vm-run] cannot read the spec file\n");
        exit(2);
    };
    let fd = open("/dev/kvm0", open_flags::O_RDWR);
    if fd < 0 {
        print("[vm-run] no /dev/kvm0: the kernel was not booted with kvmctl\n");
        exit(2);
    }
    // A guest that was just stopped may still be tearing down: wait for the slot.
    let mut waited = 0;
    loop {
        let r = write_fd(fd, &spec);
        if r >= 0 {
            break;
        }
        if r == EBUSY && waited < 150 {
            waited += 1;
            sleep_ms(200);
            continue;
        }
        print("[vm-run] the kernel refused the spec (errno ");
        print_dec((-r) as usize);
        print(")\n");
        exit(3);
    }
    print("[vm-run] guest created\n");
    let mut buf = [0u8; 512];
    let mut next_status = uptime() + STATUS_EVERY_US;
    loop {
        let n = read_fd(fd, &mut buf);
        if n > 0 {
            write_fd(1, &buf[..n as usize]);
            continue;
        }
        if n == 0 {
            break;
        }
        if n != EAGAIN {
            print("[vm-run] console read failed\n");
            exit(4);
        }
        if uptime() >= next_status {
            next_status = uptime() + STATUS_EVERY_US;
            write_fd(fd, b"status\n");
        }
        sleep_ms(50);
    }
    print("[vm-run] guest ended\n");
    // A service that ends is restarted by herd's policy; say it was not a clean stop.
    exit(1);
}

/// `sched_setaffinity(0, ..)` on herd itself; returns the mask to hand back to [`cpus_leave`]
/// (`None` when nothing was changed). x86_64 only: the AArch64 kernel treats the call as a no-op.
fn cpus_enter(mask: u64) -> Option<u64> {
    if mask == 0 {
        return None;
    }
    let prev = cpus_get();
    if cpus_set(mask) == 0 {
        Some(prev)
    } else {
        print("[herd] Warning: cpus = refused by the kernel; starting unrestricted\n");
        None
    }
}

fn cpus_leave(prev: Option<u64>) {
    if let Some(m) = prev {
        // `sched_getaffinity` reports "all cores" as an explicit full mask; setting that back is "any".
        let _ = cpus_set(m);
    }
}

fn cpus_get() -> u64 {
    let mut m = [0u64; 2];
    let n = libakuma::syscall(libakuma::syscall::SCHED_GETAFFINITY, 0, 16, m.as_mut_ptr() as u64, 0, 0, 0);
    if (n as i64) < 8 { u64::MAX } else { m[0] }
}

fn cpus_set(mask: u64) -> u64 {
    let m = [mask, 0u64];
    libakuma::syscall(libakuma::syscall::SCHED_SETAFFINITY, 0, 16, m.as_ptr() as u64, 0, 0, 0)
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
            }
        }
        if !ok {
            // core_init failed (ENOSYS on shared-kernel SMP, where every core
            // already runs this kernel): start it here like any service. This
            // used to be a second, plain spawn that dropped env, workdir,
            // console, cpus, the log and `started_at_ms` (so every exit
            // counted as a quick failure).
            print("[herd] core_init unavailable for ");
            print(name);
            print(" - starting it as a local process\n");
            let local = ServiceConfig { core: 0, ..config.clone() };
            start_service(state, name, &local);
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

    let prev_cpus = cpus_enter(config.cpus);
    let spawn_res = if let Some(spec) = &config.vm {
        spawn_vm_helper(name, spec)
    } else if !config.join_box.is_empty() {
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
        spawn_in_box(target_box_id, &config.command, &args, env.as_deref(), &config.workdir)
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
        // The service's `workdir` overrides the bundle's `process.cwd`.
        let cwd = if config.workdir.is_empty() { oci.process_cwd.as_str() } else { config.workdir.as_str() };
        let res = spawn_in_box(box_id, &command, &args, env.as_deref(), cwd);
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
        let res = spawn_in_box(box_id, &config.command, &args, env.as_deref(), &config.workdir);
        if let Some(ref r) = res {
            sys::register_box(box_id, name, &config.box_root, r.pid);
        }
        res
    } else {
        let args: Vec<&str> = config.args.iter().map(|s| s.as_str()).collect();
        let args_opt = if args.is_empty() { None } else { Some(args.as_slice()) };
        if config.console {
            // The console's process. The child's output is on the screen, so the
            // `stdout_fd` that comes back is only the exit-status handle: herd's
            // drain finds nothing in it and the service gets no log file, which
            // is the point — a shell's prompt is not log material.
            //
            // The environment is composed even with no `env =` line, unlike a
            // plain service: no environment at all would reach a shell as no
            // `PATH`, `HOME` or `TERM`, and this child is the one a person types
            // into.
            let base: Vec<String> = spec::DEFAULT_ENV.iter().map(|s| String::from(*s)).collect();
            let env = spec::compose_env(&base, &config.env);
            let refs: Vec<&str> = env.iter().map(|s| s.as_str()).collect();
            spawn_console(&config.command, args_opt, &refs)
        } else if config.workdir.is_empty() {
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
                ..Default::default()
            };
            sys::spawn_ext_env(&config.command, args_opt, env.as_deref(), None, &mut options)
        }
    };

    cpus_leave(prev_cpus);

    match spawn_res {
        Some(SpawnResult { pid, stdout_fd, .. }) => {
            if let Some(svc) = state.services.get_mut(name) {
                svc.pid = Some(pid);
                svc.stdout_fd = Some(nonblocking_stdout(stdout_fd));
                svc.state = ServiceState::Running;
                svc.restart_at_ms = None;
                svc.started_at_ms = Some(uptime() / 1000);
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
            let now_ms = uptime() / 1000;
            if let Some(svc) = state.services.get_mut(name) {
                // A supervised service is retried like a crash; see
                // `classify_spawn_failure`. `Failed` is terminal.
                let policy = Policy {
                    oneshot: svc.config.oneshot,
                    restart: svc.config.restart,
                    max_retries: svc.config.max_retries,
                };
                let outcome = classify_spawn_failure(policy, svc.restart_count);
                svc.state = ServiceState::after(outcome);
                svc.fail_streak = next_streak(svc.fail_streak, 0);
                if outcome == Outcome::Restart {
                    svc.restart_count += 1;
                    schedule_restart(svc, name, now_ms);
                }
            }
        }
    }
}

/// Put `svc` in line for a restart after `restart_delay_ms`, or, with `backoff = true`,
/// after that delay stretched by its failure streak (`herd::exit::restart_delay_ms`).
fn schedule_restart(svc: &mut SupervisedProcess, name: &str, now_ms: u64) {
    let delay = if svc.config.backoff {
        restart_delay_ms(svc.config.restart_delay_ms, svc.fail_streak)
    } else {
        svc.config.restart_delay_ms
    };
    svc.restart_at_ms = Some(now_ms + delay);
    print("[herd] Scheduling restart for ");
    print(name);
    print(" in ");
    print_dec(delay as usize);
    print(" ms\n");
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
    // Re-read `enabled/` now, not at the next 20 s tick: a service enabled or
    // edited since then would otherwise start on its previous config (the VM
    // that booted the wrong disk, 2026-11-17). An unchanged file costs a read.
    reload_config(state);
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
    svc.fail_streak = 0;
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
    /// Every service the daemon supervises, with its state.
    Status,
    /// Re-read `enabled/` now, synchronously, and start what is new.
    Reload,
}

/// `[token=<hex>] start <svc>`, `[token=<hex>] stop <svc>` or `status`,
/// whitespace-separated, nothing else. Returns the token, if any, beside the
/// request; [`authorized`] decides what it is worth.
fn parse_request(line: &str) -> Option<(Option<&str>, Request<'_>)> {
    let mut words = line.split_whitespace().peekable();
    let token = words.peek().and_then(|w| w.strip_prefix("token="));
    if token.is_some() {
        words.next();
    }
    let verb = words.next()?;
    if verb == "status" {
        return if words.next().is_none() { Some((token, Request::Status)) } else { None };
    }
    if verb == "reload" {
        return if words.next().is_none() { Some((token, Request::Reload)) } else { None };
    }
    let name = words.next()?;
    if words.next().is_some() || !valid_service_name(name) {
        return None;
    }
    match verb {
        "start" => Some((token, Request::Start(name))),
        "stop" => Some((token, Request::Stop(name))),
        _ => None,
    }
}

/// Whether a request may run: `status` always; `start`/`stop` only with the
/// daemon's token, when it has one. The comparison does not stop at the first
/// differing byte.
fn authorized(expected: Option<&str>, given: Option<&str>, request: &Request<'_>) -> bool {
    if *request == Request::Status {
        return true;
    }
    let Some(expected) = expected else { return true };
    let Some(given) = given else { return false };
    expected.len() == given.len()
        && expected.bytes().zip(given.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// A service name the CLI and the control socket accept: what can name a
/// `<name>.conf` file in one directory and travel as one word on the wire.
/// `herd add ../x` used to write outside `/etc/herd/available`.
fn valid_service_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_graphic() && b != b'/')
}

/// Make a fresh control token and publish it at [`CONTROL_TOKEN_PATH`]: a
/// temporary file, made 0600 before anything is in it, renamed over the old
/// one. Returns the token, or `None` (with a warning) if any step failed.
fn write_control_token() -> Option<String> {
    let mut raw = [0u8; CONTROL_TOKEN_BYTES];
    if !matches!(libakuma::getrandom(&mut raw), Ok(n) if n == raw.len()) {
        print("[herd] Warning: no randomness for a control token; start/stop are unauthenticated\n");
        return None;
    }
    let mut token = String::with_capacity(2 * CONTROL_TOKEN_BYTES);
    for b in raw {
        token.push_str(&format!("{b:02x}"));
    }
    let tmp = format!("{CONTROL_TOKEN_PATH}.tmp");
    let _ = libakuma::unlink(&tmp);
    let ok = write_file(&tmp, b"")
        && libakuma::chmod(&tmp, 0o600) == 0
        && write_file(&tmp, token.as_bytes())
        && libakuma::rename(&tmp, CONTROL_TOKEN_PATH) == 0;
    if !ok {
        let _ = libakuma::unlink(&tmp);
        print("[herd] Warning: cannot write ");
        print(CONTROL_TOKEN_PATH);
        print("; start/stop are unauthenticated\n");
        return None;
    }
    Some(token)
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
    let begin = uptime();
    for _ in 0..MAX_CONTROL_REQUESTS_PER_TICK {
        if uptime().saturating_sub(begin) / 1000 >= CONTROL_TICK_BUDGET_MS {
            return; // the rest wait in the backlog for the next tick
        }
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

impl ServiceState {
    const fn label(self) -> &'static str {
        match self {
            Self::Stopped => "waiting-to-start",
            Self::Running => "running",
            Self::Failed => "failed",
            Self::PendingRestart => "restart-pending",
            Self::Completed => "completed",
            Self::Halted => "halted",
            Self::Exited => "exited",
        }
    }
}

/// One line per supervised service: what `herd status` prints. Not logged: it is
/// a read, and a status poll must not push real lifecycle lines out of the ring.
fn status_report(state: &HerdState) -> String {
    let mut out = String::from("Supervised services:");
    if state.services.is_empty() {
        out.push_str("\n  (none)");
    }
    for (name, svc) in state.services.iter() {
        out.push_str(&format!("\n  {name}: {}", svc.state.label()));
        if let Some(pid) = svc.pid {
            out.push_str(&format!(" pid={pid}"));
        }
        if svc.config.vm.is_some() {
            out.push_str(" (vm)");
        }
        out.push_str(&format!(" restarts={}", svc.restart_count));
        if let Some(code) = svc.last_exit_code {
            out.push_str(&format!(" last_exit={code}"));
        }
    }
    out
}

fn handle_request(state: &mut HerdState, line: &str) -> Reply {
    let request = match parse_request(line) {
        Some((token, request)) => {
            if !authorized(state.control_token.as_deref(), token, &request) {
                print("[herd] Refused a control request without a valid token\n");
                return Reply::err(format!(
                    "not authorized: start/stop need the token in {CONTROL_TOKEN_PATH}"
                ));
            }
            Some(request)
        }
        None => None,
    };
    let reply = match request {
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
        Some(Request::Status) => return Reply::ok(status_report(state)),
        Some(Request::Reload) => {
            print("[herd] Reload requested\n");
            reload_config(state);
            start_stopped_services(state, uptime() / 1000);
            state.last_config_reload_ms = uptime() / 1000;
            Reply::ok(format!("reloaded: {} service(s) configured", state.services.len()))
        }
        // The line is not echoed back or logged: it may carry the token.
        None => Reply::err(String::from("bad request; expected `start <svc>`, `stop <svc>`, `reload` or `status`")),
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
            let ran_ms = svc.started_at_ms.take().map_or(0, |t| now_ms.saturating_sub(t));
            svc.fail_streak = next_streak(svc.fail_streak, ran_ms);
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
                    schedule_restart(svc, &name, now_ms);
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

    let max_size = if state.services.get(service_name).is_some_and(|svc| svc.config.vm.is_some()) {
        VM_MAX_LOG_SIZE
    } else {
        MAX_LOG_SIZE
    };

    // Check if rotation is needed
    if current_size + data.len() > max_size {
        // Rotate by rename. It used to read the whole log into memory (up to
        // the VM limit) and write it out again, on the supervisor's thread.
        let _ = libakuma::unlink(&log_old_path);
        let _ = libakuma::rename(&log_path, &log_old_path);

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

/// Write all of `data` to `fd`, across short writes; false on an error.
/// `write_file` used to issue one `write` and report success whatever it did.
fn write_all_fd(fd: i32, mut data: &[u8]) -> bool {
    while !data.is_empty() {
        let n = write_fd(fd, data);
        if n <= 0 {
            return false;
        }
        data = &data[(n as usize).min(data.len())..];
    }
    true
}

fn write_file(path: &str, data: &[u8]) -> bool {
    let fd = open(path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd < 0 {
        return false;
    }
    let ok = write_all_fd(fd, data);
    close(fd);
    ok
}

fn append_file(path: &str, data: &[u8]) -> bool {
    let fd = open(path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_APPEND);
    if fd < 0 {
        return false;
    }
    let ok = write_all_fd(fd, data);
    close(fd);
    ok
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
    print("  vm-run <file>  (internal) the process a `vm = true` service runs\n");
    print("  version        Show the build's git revision\n");
    print("  help           Show this help\n");
    print("\n");
    print("Without arguments, runs as daemon in foreground.\n");
}

fn cmd_status() {
    // The daemon knows what it supervises — `--service` files included, which
    // the enabled directory does not list — and in what state.
    if send_control("status", "") {
        return;
    }
    print("Daemon not reachable; services enabled on disk:\n");
    
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

fn cmd_add(name: &str) -> bool {
    let path = format!("{}/{}.conf", HERD_AVAILABLE_DIR, name);
    
    // Check if already exists
    if read_file_bytes(&path).is_some() {
        print("Service '");
        print(name);
        print("' already exists in ");
        print(HERD_AVAILABLE_DIR);
        print("/\n");
        return false;
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
        true
    } else {
        print("Error: Failed to create service configuration at ");
        print(&path);
        print("\n");
        false
    }
}

fn cmd_config(name: &str) -> bool {
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
        return true;
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
        return true;
    }
    
    print("Service '");
    print(name);
    print("' not found.\n");
    print("Check ");
    print(HERD_AVAILABLE_DIR);
    print("/ and ");
    print(HERD_ENABLED_DIR);
    print("/\n");
    false
}

fn cmd_enable(name: &str) -> bool {
    let src_path = format!("{}/{}.conf", HERD_AVAILABLE_DIR, name);
    let dst_path = format!("{}/{}.conf", HERD_ENABLED_DIR, name);
    
    // Check if already enabled
    if read_file_bytes(&dst_path).is_some() {
        print("Service '");
        print(name);
        print("' is already enabled.\n");
        return true;
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
            return false;
        }
    };
    
    // Link, do not copy: a copy froze the config at enable time, so editing
    // `available/` did nothing until a disable + enable and the service kept
    // restarting on the stale text (2026-11-17). A dangling link from a removed
    // `available/` file is replaced; if the filesystem refuses symlinks, fall
    // back to the copy.
    let _ = libakuma::unlink(&dst_path);
    let linked = libakuma::symlink(&src_path, &dst_path) == 0;
    if linked || write_file(&dst_path, &content) {
        print("Enabled service '");
        print(name);
        print(if linked { "' (linked)\n" } else { "' (copied)\n" });
        // Make the running daemon see it now instead of at its next tick.
        // Without a daemon (boot-time `herd enable`) it is picked up on start.
        if TcpStream::connect(HERD_CONTROL_ADDR).is_ok() {
            let _ = send_control("reload", "");
        } else {
            print("Service will start when the herd daemon starts.\n");
        }
        true
    } else {
        print("Error: Failed to enable service '");
        print(name);
        print("'. Could not write to ");
        print(&dst_path);
        print("\n");
        let _ = libakuma::unlink(&dst_path); // not a half-written config
        false
    }
}

fn cmd_disable(name: &str) -> bool {
    let path = format!("{}/{}.conf", HERD_ENABLED_DIR, name);
    
    // Check if enabled (`readlink` also sees a link whose target is gone).
    if read_file_bytes(&path).is_none() && libakuma::readlink(&path).is_none() {
        print("Service '");
        print(name);
        print("' is not enabled.\n");
        return true;
    }
    
    // Remove from enabled
    if libakuma::unlink(&path) == 0 {
        print("Disabled service '");
        print(name);
        print("'\n");
        if TcpStream::connect(HERD_CONTROL_ADDR).is_ok() {
            let _ = send_control("reload", "");
        }
        true
    } else {
        print("Error: Failed to delete ");
        print(&path);
        print("\n");
        false
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
    // `start`/`stop` carry the daemon's token. No token file means a daemon
    // that predates it or could not write one; it accepts the bare request.
    let token = if verb == "status" {
        None
    } else {
        read_file_string(CONTROL_TOKEN_PATH).map(|t| String::from(t.trim()))
    };
    let request = match (name.is_empty(), token) {
        (true, Some(t)) if !t.is_empty() => format!("token={t} {verb}\n"),
        (true, _) => format!("{verb}\n"),
        (false, Some(t)) if !t.is_empty() => format!("token={t} {verb} {name}\n"),
        (false, _) => format!("{verb} {name}\n"),
    };
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

fn cmd_log(name: &str) -> bool {
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
            true
        }
        None => {
            print("No log found for '");
            print(name);
            print("'\n");
            false
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

    const VM_CONF: &str = "vm = true\nvm_kernel = /boot/akuma-guest\nvm_disk = /boot/gentoo-guest.img\nvm_disk_ram = true\n\
vm_ram_mib = 768\nvm_vcpus = 2\nvm_forward = 2223:4444\nvm_cmdline = init=/usr/bin/bash initargs=/x.sh\nrestart = true\nmax_retries = 3\n";

    #[test]
    fn a_vm_conf_becomes_the_helper_service() {
        let c = parse_service_config(VM_CONF).expect("a vm service needs no `command`");
        assert_eq!(c.command, "/bin/herd");
        let spec = c.vm.expect("vm spec");
        assert_eq!((spec.vcpus, spec.ram_mib, spec.disk_ram), (2, 768, true));
        assert_eq!(spec.forwards().len(), 1);
        assert!(c.restart);
        assert_eq!(c.max_retries, 3, "the ordinary restart keys still apply to a VM");
    }

    #[test]
    fn a_bad_vm_conf_is_not_loaded() {
        assert!(parse_service_config("vm = true\nvm_vcpus = 9\nvm_kernel = /k\n").is_none());
        assert!(parse_service_config("vm = true\nvm_type = firecracker\nvm_kernel = /k\n").is_none());
        assert!(parse_service_config("vm = true\n").is_none());
    }

    #[test]
    fn a_plain_service_is_untouched_by_the_vm_keys() {
        let c = parse_service_config("command = /bin/sshd\nargs = -p 22\n").unwrap();
        assert!(c.vm.is_none());
        assert_eq!(c.command, "/bin/sshd");
    }

    #[test]
    fn a_flag_first_args_line_keeps_every_word() {
        let c = parse_service_config("command = /bin/h\nargs = --listen grpc://127.0.0.1:47200\n").unwrap();
        assert_eq!(c.args, ["--listen", "grpc://127.0.0.1:47200"]);
    }

    #[test]
    fn reload_needs_the_token_like_start() {
        assert!(!authorized(Some("ab12"), None, &Request::Reload));
        assert!(authorized(Some("ab12"), Some("ab12"), &Request::Reload));
    }

    #[test]
    fn requests_parse() {
        assert_eq!(parse_request("stop kot"), Some((None, Request::Stop("kot"))));
        assert_eq!(parse_request("  start   sshd \r"), Some((None, Request::Start("sshd"))));
        assert_eq!(parse_request("token=ab12 stop kot"), Some((Some("ab12"), Request::Stop("kot"))));
        assert_eq!(parse_request("token=ab12 status"), Some((Some("ab12"), Request::Status)));
        assert_eq!(parse_request("reload"), Some((None, Request::Reload)));
        assert_eq!(parse_request("token=ab12 reload"), Some((Some("ab12"), Request::Reload)));
        assert_eq!(parse_request("reload kot"), None);
        assert_eq!(parse_request("stop"), None);
        assert_eq!(parse_request("token=ab12"), None);
        assert_eq!(parse_request("stop kot now"), None);
        assert_eq!(parse_request("restart kot"), None);
        assert_eq!(parse_request("stop ../kot"), None);
        assert_eq!(parse_request("stop .hidden"), None);
        assert_eq!(parse_request(""), None);
    }

    #[test]
    fn start_and_stop_need_the_token_status_does_not() {
        let stop = Request::Stop("kot");
        assert!(authorized(Some("ab12"), Some("ab12"), &stop));
        assert!(!authorized(Some("ab12"), Some("ab13"), &stop));
        assert!(!authorized(Some("ab12"), Some("ab1"), &stop), "a prefix is not the token");
        assert!(!authorized(Some("ab12"), None, &stop));
        assert!(!authorized(Some("ab12"), None, &Request::Start("kot")));
        assert!(authorized(Some("ab12"), None, &Request::Status));
        assert!(authorized(None, None, &stop), "a daemon with no token accepts bare requests");
    }

    #[test]
    fn service_names_stay_in_their_directory() {
        assert!(valid_service_name("sshd"));
        assert!(valid_service_name("rump-net_2"));
        for bad in ["", "..", ".x", "a/b", "../etc/passwd", "a b", "a\tb", "a\nb"] {
            assert!(!valid_service_name(bad), "{bad:?}");
        }
        assert!(!valid_service_name(&"x".repeat(65)));
    }

    #[test]
    fn booleans_have_one_spelling_set() {
        for t in ["true", "yes", "on", "1"] {
            assert_eq!(parse_bool(t), Some(true));
        }
        for f in ["false", "no", "off", "0"] {
            assert_eq!(parse_bool(f), Some(false));
        }
        assert_eq!(parse_bool("maybe"), None);
        // `oneshot = yes` used to read as false and make a restarting service.
        let c = parse_service_config("command = /bin/x\noneshot = yes\nboxed = on\nrestart = no\n").unwrap();
        assert!(c.oneshot && c.boxed && !c.restart);
        // An unparseable value keeps the default rather than flipping it.
        let c = parse_service_config("command = /bin/x\nrestart = nope\n").unwrap();
        assert!(c.restart, "restart defaults to true");
    }

    #[test]
    fn inline_comments_are_stripped_except_from_env() {
        assert_eq!(strip_inline_comment("5000  # five seconds"), "5000");
        assert_eq!(strip_inline_comment("/bin/x#y"), "/bin/x#y", "a # inside a word is kept");
        assert_eq!(strip_inline_comment("# all comment"), "# all comment");
        let c = parse_service_config(
            "command = /bin/x   # the binary\nrestart_delay = 5000 # ms\nenv = A=b # c\n",
        )
        .unwrap();
        assert_eq!(c.command, "/bin/x");
        assert_eq!(c.restart_delay_ms, 5000, "used to fall back to the 1000 ms default");
        assert_eq!(c.env, ["A=b # c"]);
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
