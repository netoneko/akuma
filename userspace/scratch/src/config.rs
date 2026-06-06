//! Git config file parsing and writing
//!
//! Supports reading and writing .git/config files with INI-like format.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::{close, mkdir, open, open_flags, read_fd, write_fd};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Default)]
struct Section {
    name: String,
    subsection: Option<String>,
    properties: Vec<(String, String)>,
}

/// Git configuration
#[derive(Debug, Clone, Default)]
pub struct GitConfig {
    sections: Vec<Section>,
}

impl GitConfig {
    /// Load configuration from global and local configs
    /// 
    /// Checks (in order, later values override earlier):
    /// 1. /.gitconfig (global)
    /// 2. /.git/config (global alternate)
    /// 3. .git/config (local repo)
    pub fn load() -> Result<Self> {
        let mut config = GitConfig::default();
        
        // Try global configs first
        if let Ok(global) = Self::load_from_path("/.gitconfig") {
            config.merge(&global);
        }
        if let Ok(global) = Self::load_from_path("/.git/config") {
            config.merge(&global);
        }
        
        // Then local repo config (overrides global)
        let git_dir = crate::git_dir();
        if let Ok(local) = Self::load_from(&git_dir) {
            config.merge(&local);
        }
        
        Ok(config)
    }

    /// Load configuration from a specific git directory
    pub fn load_from(git_dir: &str) -> Result<Self> {
        let path = format!("{}/config", git_dir);
        Self::load_from_path(&path)
    }

    /// Load configuration from a specific file path
    fn load_from_path(path: &str) -> Result<Self> {
        let content = read_file(path)?;
        Self::parse(&content)
    }

    /// Merge another config into this one (other's values override)
    fn merge(&mut self, other: &GitConfig) {
        for other_sec in &other.sections {
            // Find matching section
            let mut found = false;
            for my_sec in &mut self.sections {
                if my_sec.name == other_sec.name && my_sec.subsection == other_sec.subsection {
                    // Merge properties
                    for (k, v) in &other_sec.properties {
                        // Update or add property
                        if let Some(pos) = my_sec.properties.iter().position(|(pk, _)| pk == k) {
                            my_sec.properties[pos] = (k.clone(), v.clone());
                        } else {
                            my_sec.properties.push((k.clone(), v.clone()));
                        }
                    }
                    found = true;
                    break;
                }
            }
            
            if !found {
                self.sections.push(other_sec.clone());
            }
        }
    }

    /// Parse config file content
    fn parse(content: &str) -> Result<Self> {
        let mut config = GitConfig::default();
        let mut current_section: Option<usize> = None;

        for line in content.lines() {
            let line = line.trim();

            // Skip empty lines and comments
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }

            // Section header: [section] or [section "subsection"]
            if line.starts_with('[') && line.ends_with(']') {
                let header = &line[1..line.len() - 1];
                let (name, subsection) = if let Some(quote_start) = header.find('"') {
                    let name = String::from(header[..quote_start].trim());
                    let subsection_part = &header[quote_start + 1..];
                    let subsection = if let Some(quote_end) = subsection_part.find('"') {
                        Some(String::from(&subsection_part[..quote_end]))
                    } else {
                        None
                    };
                    (name, subsection)
                } else {
                    (String::from(header.trim()), None)
                };
                
                let section = Section {
                    name,
                    subsection,
                    properties: Vec::new(),
                };
                
                config.sections.push(section);
                current_section = Some(config.sections.len() - 1);
                continue;
            }

            // Key = value
            if let Some(eq_pos) = line.find('=') {
                if let Some(idx) = current_section {
                    let key = line[..eq_pos].trim();
                    let value = line[eq_pos + 1..].trim();
                    config.sections[idx].properties.push((String::from(key), String::from(value)));
                }
            }
        }

        Ok(config)
    }

    /// Save configuration to .git/config
    pub fn save(&self) -> Result<()> {
        self.save_to(&crate::git_dir())
    }

    /// Save configuration to a specific git directory
    pub fn save_to(&self, git_dir: &str) -> Result<()> {
        // Ensure .git directory exists
        let _ = mkdir(git_dir);
        
        let path = format!("{}/config", git_dir);
        let mut content = String::new();

        for section in &self.sections {
            if let Some(ref sub) = section.subsection {
                content.push_str(&format!("[{} \"{}\"]\n", section.name, sub));
            } else {
                content.push_str(&format!("[{}]\n", section.name));
            }
            
            for (key, value) in &section.properties {
                content.push_str(&format!("\t{} = {}\n", key, value));
            }
        }

        write_file(&path, &content)
    }

    /// Set a config value and save to LOCAL config
    pub fn set(key: &str, value: &str) -> Result<()> {
        let git_dir = crate::git_dir();
        // Load ONLY local config (or create empty if missing)
        let mut config = Self::load_from(&git_dir).unwrap_or_default();

        // Parse key: section.subsection.key or section.key
        let parts: Vec<&str> = key.split('.').collect();
        let (section_name, subsection, prop_key) = match parts.len() {
            2 => (parts[0], None, parts[1]),
            3 => (parts[0], Some(parts[1]), parts[2]),
            _ => return Err(Error::io("invalid config key format")),
        };

        // Find or create section
        let mut section_idx = None;
        for (i, s) in config.sections.iter().enumerate() {
            if s.name == section_name && s.subsection.as_deref() == subsection {
                section_idx = Some(i);
                break;
            }
        }

        if section_idx.is_none() {
            config.sections.push(Section {
                name: String::from(section_name),
                subsection: subsection.map(String::from),
                properties: Vec::new(),
            });
            section_idx = Some(config.sections.len() - 1);
        }

        let idx = section_idx.unwrap();
        let section = &mut config.sections[idx];

        // Update or add property
        if let Some(pos) = section.properties.iter().position(|(k, _)| k == prop_key) {
            section.properties[pos] = (String::from(prop_key), String::from(value));
        } else {
            section.properties.push((String::from(prop_key), String::from(value)));
        }

        config.save_to(&git_dir)
    }

    /// Get a config value
    pub fn get(full_key: &str) -> Result<Option<String>> {
        let config = Self::load()?;
        
        let parts: Vec<&str> = full_key.split('.').collect();
        let (section_name, subsection, prop_key) = match parts.len() {
            2 => (parts[0], None, parts[1]),
            3 => (parts[0], Some(parts[1]), parts[2]),
            _ => return Ok(None),
        };

        Ok(config.get_value(section_name, subsection, prop_key).map(String::from))
    }

    pub fn get_value(&self, section: &str, subsection: Option<&str>, key: &str) -> Option<&str> {
        for s in &self.sections {
            if s.name == section && s.subsection.as_deref() == subsection {
                for (k, v) in &s.properties {
                    if k == key {
                        return Some(v);
                    }
                }
            }
        }
        None
    }

    /// Get user name, falling back to default
    pub fn get_user_name(&self) -> &str {
        self.get_value("user", None, "name").unwrap_or("Scratch User")
    }

    /// Get user email, falling back to default
    pub fn get_user_email(&self) -> &str {
        self.get_value("user", None, "email").unwrap_or("scratch@akuma.local")
    }
    
    /// Get remote URL
    pub fn get_remote_url(&self) -> Option<String> {
        self.get_value("remote", Some("origin"), "url").map(String::from)
    }
    
    /// Get credential token
    pub fn get_credential_token(&self) -> Option<String> {
        self.get_value("credential", None, "token").map(String::from)
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
