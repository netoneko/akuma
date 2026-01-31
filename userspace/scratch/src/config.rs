//! Git config file parsing and writing
//!
//! Supports reading and writing .git/config files with INI-like format.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::{close, mkdir, open, open_flags, read_fd, write_fd};

use crate::error::{Error, Result};

/// Default git directory
const GIT_DIR: &str = ".git";

/// Git configuration
#[derive(Debug, Clone, Default)]
pub struct GitConfig {
    /// Remote origin URL
    pub remote_url: Option<String>,
    /// User name for commits
    pub user_name: Option<String>,
    /// User email for commits
    pub user_email: Option<String>,
    /// Credential helper token (for push authentication)
    pub credential_token: Option<String>,
}

impl GitConfig {
    /// Load configuration from .git/config
    pub fn load() -> Result<Self> {
        Self::load_from(GIT_DIR)
    }

    /// Load configuration from a specific git directory
    pub fn load_from(git_dir: &str) -> Result<Self> {
        let path = format!("{}/config", git_dir);
        let content = read_file(&path)?;
        Self::parse(&content)
    }

    /// Parse config file content
    fn parse(content: &str) -> Result<Self> {
        let mut config = GitConfig::default();
        let mut current_section = String::new();
        let mut current_subsection: Option<String> = None;

        for line in content.lines() {
            let line = line.trim();

            // Skip empty lines and comments
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }

            // Section header: [section] or [section "subsection"]
            if line.starts_with('[') && line.ends_with(']') {
                let header = &line[1..line.len() - 1];
                if let Some(quote_start) = header.find('"') {
                    current_section = String::from(header[..quote_start].trim());
                    let subsection_part = &header[quote_start + 1..];
                    if let Some(quote_end) = subsection_part.find('"') {
                        current_subsection = Some(String::from(&subsection_part[..quote_end]));
                    }
                } else {
                    current_section = String::from(header.trim());
                    current_subsection = None;
                }
                continue;
            }

            // Key = value
            if let Some(eq_pos) = line.find('=') {
                let key = line[..eq_pos].trim();
                let value = line[eq_pos + 1..].trim();

                match (current_section.as_str(), current_subsection.as_deref(), key) {
                    ("remote", Some("origin"), "url") => {
                        config.remote_url = Some(String::from(value));
                    }
                    ("user", None, "name") => {
                        config.user_name = Some(String::from(value));
                    }
                    ("user", None, "email") => {
                        config.user_email = Some(String::from(value));
                    }
                    ("credential", None, "token") => {
                        config.credential_token = Some(String::from(value));
                    }
                    _ => {}
                }
            }
        }

        Ok(config)
    }

    /// Save configuration to .git/config
    pub fn save(&self) -> Result<()> {
        self.save_to(GIT_DIR)
    }

    /// Save configuration to a specific git directory
    pub fn save_to(&self, git_dir: &str) -> Result<()> {
        // Ensure .git directory exists
        let _ = mkdir(git_dir);
        
        let path = format!("{}/config", git_dir);
        
        let mut content = String::new();

        // Core section
        content.push_str("[core]\n");
        content.push_str("\trepositoryformatversion = 0\n");
        content.push_str("\tfilemode = true\n");
        content.push_str("\tbare = false\n");

        // Remote origin section
        if let Some(ref url) = self.remote_url {
            content.push_str("[remote \"origin\"]\n");
            content.push_str("\turl = ");
            content.push_str(url);
            content.push('\n');
            content.push_str("\tfetch = +refs/heads/*:refs/remotes/origin/*\n");
        }

        // User section
        if self.user_name.is_some() || self.user_email.is_some() {
            content.push_str("[user]\n");
            if let Some(ref name) = self.user_name {
                content.push_str("\tname = ");
                content.push_str(name);
                content.push('\n');
            }
            if let Some(ref email) = self.user_email {
                content.push_str("\temail = ");
                content.push_str(email);
                content.push('\n');
            }
        }

        // Credential section
        if let Some(ref token) = self.credential_token {
            content.push_str("[credential]\n");
            content.push_str("\ttoken = ");
            content.push_str(token);
            content.push('\n');
        }

        write_file(&path, &content)
    }

    /// Set a config value and save
    pub fn set(key: &str, value: &str) -> Result<()> {
        let mut config = Self::load().unwrap_or_default();

        match key {
            "user.name" => config.user_name = Some(String::from(value)),
            "user.email" => config.user_email = Some(String::from(value)),
            "credential.token" => config.credential_token = Some(String::from(value)),
            _ => return Err(Error::io("unknown config key")),
        }

        config.save()
    }

    /// Get a config value
    pub fn get(key: &str) -> Result<Option<String>> {
        let config = Self::load()?;

        Ok(match key {
            "user.name" => config.user_name,
            "user.email" => config.user_email,
            "credential.token" => config.credential_token,
            "remote.origin.url" => config.remote_url,
            _ => None,
        })
    }

    /// Get user name, falling back to default
    pub fn get_user_name(&self) -> &str {
        self.user_name.as_deref().unwrap_or("Scratch User")
    }

    /// Get user email, falling back to default
    pub fn get_user_email(&self) -> &str {
        self.user_email.as_deref().unwrap_or("scratch@akuma.local")
    }
}

/// Read file content as string
fn read_file(path: &str) -> Result<String> {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        return Err(Error::not_a_repository());
    }

    let mut data = Vec::new();
    let mut buf = [0u8; 1024];

    loop {
        let n = read_fd(fd, &mut buf);
        if n <= 0 {
            break;
        }
        data.extend_from_slice(&buf[..n as usize]);
    }
    close(fd);

    String::from_utf8(data).map_err(|_| Error::io("config not valid UTF-8"))
}

/// Write string content to file
fn write_file(path: &str, content: &str) -> Result<()> {
    let fd = open(path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd < 0 {
        return Err(Error::io("failed to create config file"));
    }

    let written = write_fd(fd, content.as_bytes());
    close(fd);

    if written < 0 {
        return Err(Error::io("failed to write config file"));
    }

    Ok(())
}

// Helper for lowercase comparison
trait ToLowerStr {
    fn to_lower(&self) -> String;
}

impl ToLowerStr for str {
    fn to_lower(&self) -> String {
        self.chars()
            .map(|c| {
                if c.is_ascii_uppercase() {
                    (c as u8 + 32) as char
                } else {
                    c
                }
            })
            .collect()
    }
}
