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
    pub shell: Option<String>,
    pub port: Option<u16>,
}

impl Default for SshdConfig {
    fn default() -> Self {
        Self {
            disable_key_verification: false,
            shell: None, // Default to built-in shell
            port: None,  // Default port is handled in main.rs
        }
    }
}

impl SshdConfig {
    pub fn new() -> Self {
        Self::default()
    }

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
                    self.shell = Some(String::from(value));
                }
                "port" => {
                    if let Ok(p) = value.parse::<u16>() {
                        self.port = Some(p);
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

    if let Ok(data) = read_file_to_vec(CONFIG_PATH) {
        if let Ok(content) = core::str::from_utf8(&data) {
            for line in content.lines() {
                config.parse_line(line);
            }
        }
    }

    let mut guard = CACHED_CONFIG.lock();
    *guard = Some(config.clone());
    config
}
