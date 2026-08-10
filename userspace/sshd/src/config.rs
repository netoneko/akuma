//! SSH Server Configuration
//!
//! Parses and manages the SSH server configuration file at /etc/sshd/sshd.conf

use spinning_top::Spinlock;
use libakuma::*;
use alloc::vec::Vec;
use alloc::string::String;

// ============================================================================
// Constants
// ============================================================================

const CONFIG_PATH: &str = "/etc/sshd/sshd.conf";

// ============================================================================
// Cached Configuration
// ============================================================================

static CACHED_CONFIG: Spinlock<Option<SshdConfig>> = Spinlock::new(None);

// ============================================================================
// Configuration Structure
// ============================================================================

#[derive(Debug, Clone)]
pub struct SshdConfig {
    pub disable_key_verification: bool,
    /// Path to the shell spawned for both interactive (`shell` channel
    /// request) and one-shot (`exec` channel request, `-c <cmd>`) sessions.
    /// There is no built-in fallback shell — this must be a real executable
    /// on disk. Defaults to busybox's `/bin/sh` (a devbox/bootstrap image
    /// always symlinks `/bin/sh` to busybox; see `scripts/populate_disk.sh`).
    pub shell: String,
    /// Extra argv passed to the spawned shell, after the shell path. Used to
    /// drive multicall binaries (busybox/toybox/armybox) whose applet is
    /// selected by an argument, e.g. `--shell /bin/toybox --shell-arg sh`
    /// spawns argv = ["/bin/toybox", "sh"]. Empty for a plain shell binary
    /// (the default `/bin/sh` dispatches via its own argv[0] basename).
    pub shell_args: Vec<String>,
    pub port: Option<u16>,
    /// Print the ASCII-art welcome banner on interactive `shell` sessions,
    /// mirroring the in-kernel SSH server's login banner. Enabled by default.
    pub banner: bool,
    /// Ceiling on concurrently live session processes. Each accepted connection
    /// costs one forked `sshd` child plus (once a shell/exec channel opens) one
    /// spawned shell — two entries in a `MAX_PROCESSES = 64` global table
    /// (`src/config.rs`) that every other process on the system shares. See
    /// [`DEFAULT_MAX_SESSIONS`] for why the default is what it is.
    pub max_sessions: usize,
}

/// Default [`SshdConfig::max_sessions`].
///
/// The binding constraint is not memory but the kernel's global
/// `MAX_PROCESSES = 64`. A fully-occupied session costs 2 slots (the forked
/// `sshd` child and the shell it spawns), so 24 sessions is a 48-slot
/// worst case, leaving ~16 for init, herd, the listener itself, and whatever
/// the user is actually running. Raising this past ~28 risks `fork()`
/// returning `ENOMEM` for reasons that have nothing to do with SSH — and a
/// process-table exhaustion caused by sshd is far more disruptive than a
/// connection refused at the door.
pub const DEFAULT_MAX_SESSIONS: usize = 24;

/// Default shell: busybox's multicall entry point, present on every
/// bootstrap/devbox image (`scripts/populate_disk.sh` always symlinks it).
pub const DEFAULT_SHELL: &str = "/bin/sh";

impl Default for SshdConfig {
    fn default() -> Self {
        Self {
            disable_key_verification: false,
            shell: String::from(DEFAULT_SHELL),
            shell_args: Vec::new(),
            port: None, // Default port is handled in main.rs
            banner: true,
            max_sessions: DEFAULT_MAX_SESSIONS,
        }
    }
}

impl SshdConfig {
    fn parse_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return;
        }

        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().to_lowercase();
            let value = value.trim();

            match key.as_str() {
                "disable_key_verification" => {
                    self.disable_key_verification = parse_bool(value);
                }
                "shell" => {
                    self.shell = String::from(value);
                }
                "port" => {
                    if let Ok(p) = value.parse::<u16>() {
                        self.port = Some(p);
                    }
                }
                "banner" => {
                    self.banner = parse_bool(value);
                }
                "max_sessions" => {
                    // 0 is rejected rather than treated as "unlimited": a
                    // typo'd/empty value must not silently disable the cap that
                    // protects the global process table.
                    if let Ok(n) = value.parse::<usize>()
                        && n > 0
                    {
                        self.max_sessions = n;
                    }
                }
                _ => {}
            }
        }
    }
}

fn parse_bool(s: &str) -> bool {
    let s = s.trim().to_lowercase();
    matches!(s.as_str(), "true" | "yes" | "1" | "on")
}

pub fn get_config() -> SshdConfig {
    let guard = CACHED_CONFIG.lock();
    guard.clone().unwrap_or_default()
}

/// Helper to read file to Vec<u8>
fn read_file_to_vec(path: &str) -> Result<Vec<u8>, i32> {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 { return Err(fd); }
    
    let mut result = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = read_fd(fd, &mut buf);
        if n < 0 { close(fd); return Err(n as i32); }
        if n == 0 { break; }
        result.extend_from_slice(&buf[..n as usize]);
    }
    close(fd);
    Ok(result)
}

pub async fn load_config() -> SshdConfig {
    let mut config = SshdConfig::default();

    if let Ok(data) = read_file_to_vec(CONFIG_PATH)
        && let Ok(content) = core::str::from_utf8(&data)
    {
        for line in content.lines() {
            config.parse_line(line);
        }
    }

    let mut guard = CACHED_CONFIG.lock();
    *guard = Some(config.clone());
    config
}
