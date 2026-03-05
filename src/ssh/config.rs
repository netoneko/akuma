//! SSH Server Configuration — kernel wrapper.
//!
//! Re-exports `SshdConfig` from `akuma_ssh::config` and provides async
//! filesystem loading/caching.

use crate::async_fs;
use spinning_top::Spinlock;

pub use akuma_ssh::config::SshdConfig;

const CONFIG_PATH: &str = "/etc/sshd/sshd.conf";

static CACHED_CONFIG: Spinlock<Option<SshdConfig>> = Spinlock::new(None);

/// Get the cached configuration (must call `load_config` first!).
/// Returns the cached config, or default if not loaded yet.
pub fn get_config() -> SshdConfig {
    let guard = CACHED_CONFIG.lock();
    guard.clone().unwrap_or_default()
}

/// Load the SSH server configuration from `/etc/sshd/sshd.conf`.
/// Called once at SSH server startup; subsequent calls return the cache.
pub async fn load_config() -> SshdConfig {
    {
        let guard = CACHED_CONFIG.lock();
        if let Some(ref config) = *guard {
            return config.clone();
        }
    }

    let config = if !async_fs::exists(CONFIG_PATH).await {
        log("[SSH Config] No config file found, using defaults\n");
        SshdConfig::default()
    } else {
        match async_fs::read_to_string(CONFIG_PATH).await {
            Ok(content) => {
                let c = SshdConfig::parse(&content);
                safe_print!(
                    256,
                    "[SSH Config] Loaded config: disable_key_verification={}\n",
                    c.disable_key_verification
                );
                c
            }
            Err(e) => {
                safe_print!(
                    256,
                    "[SSH Config] Failed to read config: {}, using defaults\n",
                    e
                );
                SshdConfig::default()
            }
        }
    };

    {
        let mut guard = CACHED_CONFIG.lock();
        *guard = Some(config.clone());
    }

    config
}

/// Create a default config file if it doesn't exist.
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
        safe_print!(
            256,
            "[SSH Config] Failed to create default config: {}\n",
            e
        );
    } else {
        safe_print!(
            256,
            "[SSH Config] Created default config at {}\n",
            CONFIG_PATH
        );
    }
}

fn log(msg: &str) {
    safe_print!(256, "{}", msg);
}
