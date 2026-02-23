//! SSH Server Configuration
//!
//! Parses and manages the SSH server configuration file at /etc/sshd/sshd.conf
//!
//! Configuration is loaded once at SSH server startup and cached.
//! This avoids filesystem access on every connection.

use crate::async_fs;
use crate::console;
use spinning_top::Spinlock;

// ============================================================================
// Constants
// ============================================================================

const CONFIG_PATH: &str = "/etc/sshd/sshd.conf";

// ============================================================================
// Cached Configuration
// ============================================================================

/// Cached configuration loaded at server startup
static CACHED_CONFIG: Spinlock<Option<SshdConfig>> = Spinlock::new(None);

// ============================================================================
// Configuration Structure
// ============================================================================

/// SSH server configuration
#[derive(Debug, Clone)]
pub struct SshdConfig {
    /// If true, accept any authentication without verifying keys
    pub disable_key_verification: bool,
    /// Default shell path
    pub shell: Option<alloc::string::String>,
}

impl Default for SshdConfig {
    fn default() -> Self {
        Self {
            disable_key_verification: false,
            shell: None,
        }
    }
}

impl SshdConfig {
    /// Create a new config with default values
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a config line and update the config
    fn parse_line(&mut self, line: &str) {
        let line = line.trim();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') {
            return;
        }

        // Parse key = value format
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().to_lowercase();
            let value = value.trim();

            match key.as_str() {
                "disable_key_verification" => {
                    self.disable_key_verification = parse_bool(value);
                }
                "shell" => {
                    self.shell = Some(alloc::string::String::from(value));
                }
                _ => {
                    log(&alloc::format!(
                        "[SSH Config] Unknown config key: {}\n",
                        key
                    ));
                }
            }
        }
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Parse a boolean value from a string
fn parse_bool(s: &str) -> bool {
    let s = s.trim().to_lowercase();
    matches!(s.as_str(), "true" | "yes" | "1" | "on")
}

// ============================================================================
// Config Loading
// ============================================================================

/// Get the cached configuration (must call load_config first!)
/// 
/// Returns the cached config, or default if not loaded yet.
/// This is synchronous and safe to call from any thread.
pub fn get_config() -> SshdConfig {
    let guard = CACHED_CONFIG.lock();
    guard.clone().unwrap_or_default()
}

/// Load the SSH server configuration from /etc/sshd/sshd.conf
/// 
/// This should be called once at SSH server startup.
/// The configuration is cached for subsequent get_config() calls.
/// Returns default config if file doesn't exist or can't be parsed.
pub async fn load_config() -> SshdConfig {
    // Check if already cached
    {
        let guard = CACHED_CONFIG.lock();
        if let Some(ref config) = *guard {
            return config.clone();
        }
    }

    let mut config = SshdConfig::default();

    if !async_fs::exists(CONFIG_PATH).await {
        log("[SSH Config] No config file found, using defaults\n");
    } else {
        match async_fs::read_to_string(CONFIG_PATH).await {
            Ok(content) => {
                for line in content.lines() {
                    config.parse_line(line);
                }
                log(&alloc::format!(
                    "[SSH Config] Loaded config: disable_key_verification={}\n",
                    config.disable_key_verification
                ));
            }
            Err(e) => {
                log(&alloc::format!(
                    "[SSH Config] Failed to read config: {}, using defaults\n",
                    e
                ));
            }
        }
    }

    // Cache the config
    {
        let mut guard = CACHED_CONFIG.lock();
        *guard = Some(config.clone());
    }

    config
}

/// Create a default config file if it doesn't exist
pub async fn ensure_default_config() {
    if async_fs::exists(CONFIG_PATH).await {
        return;
    }

    let default_content = r#"# SSH Server Configuration
# Edit this file to customize SSH server behavior

# Set to true to accept any authentication without verifying keys
# WARNING: This is insecure and should only be used for testing
disable_key_verification = false
"#;

    if let Err(e) = async_fs::write_file(CONFIG_PATH, default_content.as_bytes()).await {
        log(&alloc::format!(
            "[SSH Config] Failed to create default config: {}\n",
            e
        ));
    } else {
        log(&alloc::format!(
            "[SSH Config] Created default config at {}\n",
            CONFIG_PATH
        ));
    }
}

// ============================================================================
// Logging
// ============================================================================

fn log(msg: &str) {
    safe_print!(256, "{}", msg);
}
