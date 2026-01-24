//! Herd Process Supervisor
//!
//! A kernel-level process supervisor that manages background services,
//! polls their stdout to log files, auto-restarts on failure, and
//! reloads config every 20 seconds.
//!
//! Named "herd" because herding cats is an apt metaphor for managing processes.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use embassy_time::{Duration, Timer};
use spinning_top::Spinlock;

use crate::console;
use crate::process::ProcessChannel;
use crate::timer;

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
// Service State
// ============================================================================

/// State of a supervised service
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// Service is stopped (not running)
    Stopped,
    /// Service is running
    Running,
    /// Service failed and exceeded max retries
    Failed,
    /// Service is pending restart (waiting for delay)
    PendingRestart,
}

impl ServiceState {
    /// Get a human-readable string for the state
    pub fn as_str(&self) -> &'static str {
        match self {
            ServiceState::Stopped => "stopped",
            ServiceState::Running => "running",
            ServiceState::Failed => "failed",
            ServiceState::PendingRestart => "restarting",
        }
    }
}

// ============================================================================
// Service Configuration
// ============================================================================

/// Configuration for a supervised service (parsed from config file)
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// Command to execute (e.g., "/bin/httpd")
    pub command: String,
    /// Command arguments (e.g., ["--port", "80"])
    pub args: Vec<String>,
    /// Delay before restart in milliseconds
    pub restart_delay_ms: u64,
    /// Maximum restart attempts (0 = infinite)
    pub max_retries: u32,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            restart_delay_ms: DEFAULT_RESTART_DELAY_MS,
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }
}

// ============================================================================
// Supervised Process
// ============================================================================

/// State for a supervised process
pub struct SupervisedProcess {
    /// Service name (e.g., "httpd")
    pub name: String,
    /// Parsed configuration
    pub config: ServiceConfig,
    /// Thread ID if running (None if stopped)
    pub thread_id: Option<usize>,
    /// Process channel for stdout polling
    pub channel: Option<Arc<ProcessChannel>>,
    /// Current service state
    pub state: ServiceState,
    /// Number of restart attempts since last successful start
    pub restart_count: u32,
    /// Last exit code (if any)
    pub last_exit_code: Option<i32>,
    /// Timestamp when last started (uptime_us)
    pub last_started_us: u64,
    /// Timestamp when restart is scheduled (uptime_us) - for pending restarts
    pub restart_scheduled_us: Option<u64>,
    /// Current log file size (for rotation)
    pub log_size: usize,
}

impl SupervisedProcess {
    /// Create a new supervised process with the given name and config
    pub fn new(name: String, config: ServiceConfig) -> Self {
        Self {
            name,
            config,
            thread_id: None,
            channel: None,
            state: ServiceState::Stopped,
            restart_count: 0,
            last_exit_code: None,
            last_started_us: 0,
            restart_scheduled_us: None,
            log_size: 0,
        }
    }
}

// ============================================================================
// Herd State
// ============================================================================

/// Global herd supervisor state
pub struct HerdState {
    /// Map of service name to supervised process
    pub services: BTreeMap<String, SupervisedProcess>,
    /// Last config reload timestamp (uptime_us)
    pub last_config_reload_us: u64,
}

impl HerdState {
    /// Create a new empty herd state
    pub fn new() -> Self {
        Self {
            services: BTreeMap::new(),
            last_config_reload_us: 0,
        }
    }
}

impl Default for HerdState {
    fn default() -> Self {
        Self::new()
    }
}

/// Global herd state protected by spinlock
/// Only accessed from the async supervisor task and shell commands
static HERD_STATE: Spinlock<Option<HerdState>> = Spinlock::new(None);

/// Flag indicating if the supervisor is initialized
static HERD_INITIALIZED: AtomicBool = AtomicBool::new(false);

// ============================================================================
// Public API for Shell Commands
// ============================================================================

/// Check if herd is initialized
pub fn is_initialized() -> bool {
    HERD_INITIALIZED.load(Ordering::Acquire)
}

/// Get a list of all services with their status
pub fn list_services() -> Vec<(String, ServiceState, Option<i32>)> {
    let state = HERD_STATE.lock();
    if let Some(ref state) = *state {
        state
            .services
            .iter()
            .map(|(name, svc)| (name.clone(), svc.state, svc.last_exit_code))
            .collect()
    } else {
        Vec::new()
    }
}

/// Get detailed info about a specific service
pub fn get_service_info(name: &str) -> Option<ServiceInfo> {
    let state = HERD_STATE.lock();
    state.as_ref().and_then(|s| {
        s.services.get(name).map(|svc| ServiceInfo {
            name: svc.name.clone(),
            state: svc.state,
            config: svc.config.clone(),
            thread_id: svc.thread_id,
            restart_count: svc.restart_count,
            last_exit_code: svc.last_exit_code,
            last_started_us: svc.last_started_us,
        })
    })
}

/// Detailed service info for shell commands
#[derive(Debug, Clone)]
pub struct ServiceInfo {
    pub name: String,
    pub state: ServiceState,
    pub config: ServiceConfig,
    pub thread_id: Option<usize>,
    pub restart_count: u32,
    pub last_exit_code: Option<i32>,
    pub last_started_us: u64,
}

/// Request to start a service (called from shell)
pub fn request_start(name: &str) -> Result<(), &'static str> {
    let mut state = HERD_STATE.lock();
    if let Some(ref mut state) = *state {
        if let Some(svc) = state.services.get_mut(name) {
            if svc.state == ServiceState::Running {
                return Err("Service is already running");
            }
            // Reset state for manual start
            svc.state = ServiceState::Stopped;
            svc.restart_count = 0;
            svc.restart_scheduled_us = None;
            // The supervisor loop will pick this up and start it
            Ok(())
        } else {
            Err("Service not found")
        }
    } else {
        Err("Herd not initialized")
    }
}

/// Request to stop a service (called from shell)
pub fn request_stop(name: &str) -> Result<(), &'static str> {
    // Copy thread_id while holding lock, then release before kill
    let thread_id = {
        let mut state = HERD_STATE.lock();
        if let Some(ref mut state) = *state {
            if let Some(svc) = state.services.get_mut(name) {
                if svc.state != ServiceState::Running {
                    return Err("Service is not running");
                }
                // Mark as stopped so supervisor doesn't restart it
                svc.state = ServiceState::Stopped;
                svc.restart_scheduled_us = None;
                svc.thread_id
            } else {
                return Err("Service not found");
            }
        } else {
            return Err("Herd not initialized");
        }
    };

    // Kill process outside of lock to avoid deadlock
    if let Some(tid) = thread_id {
        // Find pid from thread_id
        if let Some(pid) = crate::process::find_pid_by_thread(tid) {
            let _ = crate::process::kill_process(pid);
        }
    }
    Ok(())
}

/// Add a new service from config file
pub fn add_service(name: &str, config: ServiceConfig) {
    let mut state = HERD_STATE.lock();
    if let Some(ref mut state) = *state {
        if !state.services.contains_key(name) {
            let svc = SupervisedProcess::new(name.to_string(), config);
            state.services.insert(name.to_string(), svc);
        }
    }
}

/// Remove a service (must be stopped first)
pub fn remove_service(name: &str) -> Result<(), &'static str> {
    let mut state = HERD_STATE.lock();
    if let Some(ref mut state) = *state {
        if let Some(svc) = state.services.get(name) {
            if svc.state == ServiceState::Running {
                return Err("Cannot remove running service");
            }
        }
        state.services.remove(name);
        Ok(())
    } else {
        Err("Herd not initialized")
    }
}

// ============================================================================
// Config Parser
// ============================================================================

/// Parse a service config file content
/// Format:
/// ```text
/// command=/bin/httpd
/// args=--port 80 --host 0.0.0.0
/// restart_delay=1000
/// max_retries=5
/// ```
pub fn parse_service_config(content: &str) -> Result<ServiceConfig, &'static str> {
    let mut config = ServiceConfig::default();

    for line in content.lines() {
        let line = line.trim();
        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();

            match key {
                "command" => {
                    config.command = value.to_string();
                }
                "args" => {
                    // Split args by spaces, respecting quotes would be nice but keep it simple
                    config.args = value
                        .split_whitespace()
                        .map(|s| s.to_string())
                        .collect();
                }
                "restart_delay" => {
                    config.restart_delay_ms = value.parse().unwrap_or(DEFAULT_RESTART_DELAY_MS);
                }
                "max_retries" => {
                    config.max_retries = value.parse().unwrap_or(DEFAULT_MAX_RETRIES);
                }
                _ => {
                    // Unknown key, ignore
                }
            }
        }
    }

    if config.command.is_empty() {
        return Err("Missing 'command' in config");
    }

    Ok(config)
}

// ============================================================================
// Log Rotation
// ============================================================================

/// Append data to a service log file with rotation
pub async fn append_to_log(service_name: &str, data: &[u8]) -> Result<usize, &'static str> {
    if data.is_empty() {
        return Ok(0);
    }

    let log_path = format!("{}/{}.log", HERD_LOG_DIR, service_name);
    let log_old_path = format!("{}/{}.log.old", HERD_LOG_DIR, service_name);

    // Get current log size from state
    let current_size = {
        let state = HERD_STATE.lock();
        state
            .as_ref()
            .and_then(|s| s.services.get(service_name))
            .map(|svc| svc.log_size)
            .unwrap_or(0)
    };

    // Check if rotation is needed
    if current_size + data.len() > MAX_LOG_SIZE {
        // Rotate: read current log, write to .old, then write new data
        // (We don't have rename, so we do copy+delete)
        if let Ok(old_data) = crate::async_fs::read_file(&log_path).await {
            let _ = crate::async_fs::write_file(&log_old_path, &old_data).await;
        }

        // Reset log size in state
        {
            let mut state = HERD_STATE.lock();
            if let Some(ref mut state) = *state {
                if let Some(svc) = state.services.get_mut(service_name) {
                    svc.log_size = 0;
                }
            }
        }

        // Write data to new log file (overwrite)
        crate::async_fs::write_file(&log_path, data)
            .await
            .map_err(|_| "Failed to write log")?;
    } else {
        // Append to existing log
        crate::async_fs::append_file(&log_path, data)
            .await
            .map_err(|_| "Failed to append to log")?;
    }

    // Update log size in state
    {
        let mut state = HERD_STATE.lock();
        if let Some(ref mut state) = *state {
            if let Some(svc) = state.services.get_mut(service_name) {
                if current_size + data.len() > MAX_LOG_SIZE {
                    svc.log_size = data.len();
                } else {
                    svc.log_size += data.len();
                }
            }
        }
    }

    Ok(data.len())
}

// ============================================================================
// Supervisor Core Functions
// ============================================================================

/// Initialize herd directories
async fn ensure_directories() {
    // Create /etc/herd directories
    let _ = crate::async_fs::create_dir("/etc").await;
    let _ = crate::async_fs::create_dir("/etc/herd").await;
    let _ = crate::async_fs::create_dir(HERD_AVAILABLE_DIR).await;
    let _ = crate::async_fs::create_dir(HERD_ENABLED_DIR).await;

    // Create /var/log/herd directory
    let _ = crate::async_fs::create_dir("/var").await;
    let _ = crate::async_fs::create_dir("/var/log").await;
    let _ = crate::async_fs::create_dir(HERD_LOG_DIR).await;
}

/// Reload configuration from /etc/herd/enabled/
async fn reload_config() {
    console::print("[herd] Reloading configuration...\n");

    // List enabled services
    let entries = match crate::async_fs::list_dir(HERD_ENABLED_DIR).await {
        Ok(entries) => entries,
        Err(_) => {
            console::print("[herd] Warning: Cannot read enabled directory\n");
            return;
        }
    };

    // Track which services we found
    let mut found_services: Vec<String> = Vec::new();

    for entry in entries {
        // Only process .conf files
        if !entry.name.ends_with(".conf") {
            continue;
        }

        // Extract service name from filename
        let service_name = entry.name.trim_end_matches(".conf").to_string();
        found_services.push(service_name.clone());

        // Read and parse config
        let config_path = format!("{}/{}", HERD_ENABLED_DIR, entry.name);
        let content = match crate::async_fs::read_file(&config_path).await {
            Ok(data) => match core::str::from_utf8(&data) {
                Ok(s) => s.to_string(),
                Err(_) => continue,
            },
            Err(_) => continue,
        };

        match parse_service_config(&content) {
            Ok(config) => {
                // Add or update service
                let mut state = HERD_STATE.lock();
                if let Some(ref mut state) = *state {
                    if let Some(svc) = state.services.get_mut(&service_name) {
                        // Update config for existing service
                        svc.config = config;
                    } else {
                        // Add new service
                        let svc = SupervisedProcess::new(service_name.clone(), config);
                        state.services.insert(service_name, svc);
                    }
                }
            }
            Err(e) => {
                crate::safe_print!(64, "[herd] Error parsing {}: {}\n", entry.name, e);
            }
        }
    }

    // Remove services that are no longer in enabled (and not running)
    {
        let mut state = HERD_STATE.lock();
        if let Some(ref mut state) = *state {
            let to_remove: Vec<String> = state
                .services
                .iter()
                .filter(|(name, svc)| {
                    !found_services.contains(name) && svc.state != ServiceState::Running
                })
                .map(|(name, _)| name.clone())
                .collect();

            for name in to_remove {
                state.services.remove(&name);
            }
        }
    }

    // Update reload timestamp
    {
        let mut state = HERD_STATE.lock();
        if let Some(ref mut state) = *state {
            state.last_config_reload_us = timer::uptime_us();
        }
    }
}

/// Start all enabled services that are stopped
async fn start_enabled_services() {
    // Collect services to start (copy data out while holding lock)
    let to_start: Vec<(String, ServiceConfig)> = {
        let state = HERD_STATE.lock();
        if let Some(ref state) = *state {
            state
                .services
                .iter()
                .filter(|(_, svc)| svc.state == ServiceState::Stopped)
                .map(|(name, svc)| (name.clone(), svc.config.clone()))
                .collect()
        } else {
            Vec::new()
        }
    };

    for (name, config) in to_start {
        start_service_internal(&name, &config).await;
    }
}

/// Internal function to start a service
async fn start_service_internal(name: &str, config: &ServiceConfig) {
    crate::safe_print!(64, "[herd] Starting service: {}\n", name);

    // Build args for spawn
    let args: Vec<&str> = config.args.iter().map(|s| s.as_str()).collect();
    let args_opt = if args.is_empty() { None } else { Some(args.as_slice()) };

    // Spawn the process
    match crate::process::spawn_process_with_channel(&config.command, args_opt, None) {
        Ok((thread_id, channel)) => {
            // Update state with new process info
            let mut state = HERD_STATE.lock();
            if let Some(ref mut state) = *state {
                if let Some(svc) = state.services.get_mut(name) {
                    svc.thread_id = Some(thread_id);
                    svc.channel = Some(channel);
                    svc.state = ServiceState::Running;
                    svc.last_started_us = timer::uptime_us();
                    svc.restart_scheduled_us = None;
                    console::print("[herd] Service started: ");
                    console::print(name);
                    console::print("\n");
                }
            }
        }
        Err(e) => {
            crate::safe_print!(128, "[herd] Failed to start {}: {}\n", name, e);
            // Mark as failed
            let mut state = HERD_STATE.lock();
            if let Some(ref mut state) = *state {
                if let Some(svc) = state.services.get_mut(name) {
                    svc.state = ServiceState::Failed;
                }
            }
        }
    }
}

/// Poll stdout from all running services and write to logs
async fn poll_all_stdout() {
    // Collect data from channels (copy out while holding lock briefly)
    let outputs: Vec<(String, Vec<u8>)> = {
        let state = HERD_STATE.lock();
        if let Some(ref state) = *state {
            state
                .services
                .iter()
                .filter_map(|(name, svc)| {
                    svc.channel.as_ref().and_then(|ch| {
                        ch.try_read().map(|data| (name.clone(), data))
                    })
                })
                .collect()
        } else {
            Vec::new()
        }
    };

    // Write to log files (outside of lock)
    for (name, data) in outputs {
        if !data.is_empty() {
            let _ = append_to_log(&name, &data).await;
        }
    }
}

/// Check for exited processes and handle restarts
async fn check_process_exits() {
    let now_us = timer::uptime_us();

    // First pass: collect info about exited processes
    let exited: Vec<(String, i32, ServiceConfig, u32, u32)> = {
        let mut state = HERD_STATE.lock();
        if let Some(ref mut state) = *state {
            state
                .services
                .iter_mut()
                .filter_map(|(name, svc)| {
                    if svc.state == ServiceState::Running {
                        if let Some(ref channel) = svc.channel {
                            if channel.has_exited() {
                                let exit_code = channel.exit_code();
                                // Drain remaining output
                                let remaining = channel.read_all();

                                // Clean up channel reference
                                svc.channel = None;
                                svc.thread_id = None;
                                svc.last_exit_code = Some(exit_code);

                                return Some((
                                    name.clone(),
                                    exit_code,
                                    svc.config.clone(),
                                    svc.restart_count,
                                    svc.config.max_retries,
                                ));
                            }
                        }
                    }
                    None
                })
                .collect()
        } else {
            Vec::new()
        }
    };

    // Process exits and schedule restarts
    for (name, exit_code, config, restart_count, max_retries) in exited {
        crate::safe_print!(64, "[herd] Service {} exited with code {}\n", name, exit_code);

        // Only restart on non-zero exit code
        if exit_code != 0 {
            // Check if we should restart
            let should_restart = max_retries == 0 || restart_count < max_retries;

            if should_restart {
                // Schedule restart
                let restart_at = now_us + (config.restart_delay_ms * 1000);
                let mut state = HERD_STATE.lock();
                if let Some(ref mut state) = *state {
                    if let Some(svc) = state.services.get_mut(&name) {
                        svc.state = ServiceState::PendingRestart;
                        svc.restart_count += 1;
                        svc.restart_scheduled_us = Some(restart_at);
                        crate::safe_print!(
                            96,
                            "[herd] Scheduling restart for {} (attempt {}/{})\n",
                            name,
                            svc.restart_count,
                            if max_retries == 0 { "inf".to_string() } else { max_retries.to_string() }
                        );
                    }
                }
            } else {
                // Max retries exceeded
                let mut state = HERD_STATE.lock();
                if let Some(ref mut state) = *state {
                    if let Some(svc) = state.services.get_mut(&name) {
                        svc.state = ServiceState::Failed;
                        crate::safe_print!(
                            64,
                            "[herd] Service {} failed after {} retries\n",
                            name,
                            restart_count
                        );
                    }
                }
            }
        } else {
            // Clean exit (code 0) - just mark as stopped
            let mut state = HERD_STATE.lock();
            if let Some(ref mut state) = *state {
                if let Some(svc) = state.services.get_mut(&name) {
                    svc.state = ServiceState::Stopped;
                    svc.restart_count = 0; // Reset on clean exit
                }
            }
        }
    }

    // Second pass: process pending restarts
    let to_restart: Vec<(String, ServiceConfig)> = {
        let state = HERD_STATE.lock();
        if let Some(ref state) = *state {
            state
                .services
                .iter()
                .filter_map(|(name, svc)| {
                    if svc.state == ServiceState::PendingRestart {
                        if let Some(restart_at) = svc.restart_scheduled_us {
                            if now_us >= restart_at {
                                return Some((name.clone(), svc.config.clone()));
                            }
                        }
                    }
                    None
                })
                .collect()
        } else {
            Vec::new()
        }
    };

    for (name, config) in to_restart {
        start_service_internal(&name, &config).await;
    }
}

// ============================================================================
// Main Supervisor Loop
// ============================================================================

/// Main herd supervisor async task
/// This runs as part of the kernel's main async loop
pub async fn herd_supervisor() -> ! {
    console::print("[herd] Supervisor starting...\n");

    // Initialize state
    {
        let mut state = HERD_STATE.lock();
        *state = Some(HerdState::new());
    }
    HERD_INITIALIZED.store(true, Ordering::Release);

    // Ensure directories exist
    ensure_directories().await;

    // Initial config load
    reload_config().await;

    // Start all enabled services
    start_enabled_services().await;

    let mut last_config_reload_us = timer::uptime_us();

    loop {
        // 1. Poll stdout from all running services
        poll_all_stdout().await;

        // 2. Check for exited processes and handle restarts
        check_process_exits().await;

        // 3. Reload config every 20 seconds
        let now_us = timer::uptime_us();
        if now_us.saturating_sub(last_config_reload_us) >= CONFIG_RELOAD_INTERVAL_MS * 1000 {
            console::print("[herd] Periodic config reload\n");
            reload_config().await;
            last_config_reload_us = now_us;
        }

        // 4. Brief sleep to avoid busy-looping
        Timer::after(Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
}
