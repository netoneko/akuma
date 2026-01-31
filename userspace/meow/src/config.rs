//! Configuration module for Meow
//!
//! Handles loading and saving configuration from /etc/meow/config
//! Uses a simple key-value format (no TOML parser needed for no_std)

use alloc::string::String;
use alloc::vec::Vec;

use libakuma::{open, close, read_fd, write_fd, fstat, mkdir, open_flags};

/// API type for the provider
#[derive(Debug, Clone, PartialEq)]
pub enum ApiType {
    Ollama,
    OpenAI,
}

impl ApiType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApiType::Ollama => "ollama",
            ApiType::OpenAI => "openai",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "ollama" => Some(ApiType::Ollama),
            "openai" => Some(ApiType::OpenAI),
            _ => None,
        }
    }
}

impl Default for ApiType {
    fn default() -> Self {
        ApiType::Ollama
    }
}

/// A configured AI provider
#[derive(Debug, Clone)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    pub api_type: ApiType,
    pub api_key: Option<String>,
}

impl Provider {
    /// Create a new Ollama provider with default settings
    pub fn ollama_default() -> Self {
        Provider {
            name: String::from("ollama"),
            base_url: String::from("http://10.0.2.2:11434"),
            api_type: ApiType::Ollama,
            api_key: None,
        }
    }

    /// Get the host and port from the base_url
    pub fn host_port(&self) -> Option<(String, u16)> {
        let url = self.base_url
            .trim_start_matches("http://")
            .trim_start_matches("https://");
        
        let (host_port, _path) = match url.find('/') {
            Some(pos) => (&url[..pos], &url[pos..]),
            None => (url, ""),
        };

        if let Some(pos) = host_port.rfind(':') {
            let host = &host_port[..pos];
            if let Ok(port) = host_port[pos + 1..].parse::<u16>() {
                return Some((String::from(host), port));
            }
        }

        // Default ports
        let default_port = if self.base_url.starts_with("https://") { 443 } else { 80 };
        Some((String::from(host_port), default_port))
    }

    /// Check if this provider uses HTTPS
    pub fn is_https(&self) -> bool {
        self.base_url.starts_with("https://")
    }

    /// Get the base path from the URL (e.g., "/openai/v1" from "https://api.groq.com/openai/v1")
    pub fn base_path(&self) -> &str {
        let url = self.base_url
            .trim_start_matches("http://")
            .trim_start_matches("https://");
        match url.find('/') {
            Some(pos) => &url[pos..],
            None => "",
        }
    }
}

/// Main configuration structure
#[derive(Debug, Clone)]
pub struct Config {
    pub current_provider: String,
    pub current_model: String,
    pub providers: Vec<Provider>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            current_provider: String::from("ollama"),
            current_model: String::from("gemma3:27b"),
            providers: alloc::vec![Provider::ollama_default()],
        }
    }
}

/// Config file path
const CONFIG_PATH: &str = "/etc/meow/config";
const CONFIG_DIR: &str = "/etc/meow";

impl Config {
    /// Load configuration from disk
    /// Returns default config if file doesn't exist
    pub fn load() -> Self {
        let fd = open(CONFIG_PATH, open_flags::O_RDONLY);
        if fd < 0 {
            return Self::default();
        }

        // Get file size
        let stat = match fstat(fd) {
            Ok(s) => s,
            Err(_) => {
                close(fd);
                return Self::default();
            }
        };

        let size = stat.st_size as usize;
        if size == 0 || size > 16 * 1024 {
            close(fd);
            return Self::default();
        }

        let mut buf = alloc::vec![0u8; size];
        let bytes_read = read_fd(fd, &mut buf);
        close(fd);

        if bytes_read <= 0 {
            return Self::default();
        }

        let content = match core::str::from_utf8(&buf[..bytes_read as usize]) {
            Ok(s) => s,
            Err(_) => return Self::default(),
        };

        Self::parse(content)
    }

    /// Parse config from string content
    fn parse(content: &str) -> Self {
        let mut config = Config {
            current_provider: String::from("ollama"),
            current_model: String::from("gemma3:27b"),
            providers: Vec::new(),
        };

        let mut current_provider: Option<Provider> = None;

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // Check for section header [provider:name]
            if line.starts_with("[provider:") && line.ends_with(']') {
                // Save previous provider if any
                if let Some(p) = current_provider.take() {
                    config.providers.push(p);
                }

                let name = &line[10..line.len() - 1];
                current_provider = Some(Provider {
                    name: String::from(name),
                    base_url: String::new(),
                    api_type: ApiType::Ollama,
                    api_key: None,
                });
                continue;
            }

            // Parse key=value
            if let Some(eq_pos) = line.find('=') {
                let key = line[..eq_pos].trim();
                let value = line[eq_pos + 1..].trim();

                if let Some(ref mut p) = current_provider {
                    // Inside a provider section
                    match key {
                        "base_url" => p.base_url = String::from(value),
                        "api_type" => {
                            if let Some(t) = ApiType::from_str(value) {
                                p.api_type = t;
                            }
                        }
                        "api_key" => {
                            if !value.is_empty() {
                                p.api_key = Some(String::from(value));
                            }
                        }
                        _ => {}
                    }
                } else {
                    // Global settings
                    match key {
                        "current_provider" => config.current_provider = String::from(value),
                        "current_model" => config.current_model = String::from(value),
                        _ => {}
                    }
                }
            }
        }

        // Save last provider
        if let Some(p) = current_provider {
            config.providers.push(p);
        }

        // Ensure we have at least the default provider
        if config.providers.is_empty() {
            config.providers.push(Provider::ollama_default());
        }

        config
    }

    /// Save configuration to disk
    pub fn save(&self) -> Result<(), &'static str> {
        // Create directory if needed
        mkdir(CONFIG_DIR);

        let content = self.serialize();

        let fd = open(CONFIG_PATH, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
        if fd < 0 {
            return Err("Failed to open config file for writing");
        }

        let bytes_written = write_fd(fd, content.as_bytes());
        close(fd);

        if bytes_written < 0 {
            return Err("Failed to write config file");
        }

        Ok(())
    }

    /// Serialize config to string
    fn serialize(&self) -> String {
        let mut content = String::new();

        // Global settings
        content.push_str("current_provider=");
        content.push_str(&self.current_provider);
        content.push('\n');

        content.push_str("current_model=");
        content.push_str(&self.current_model);
        content.push_str("\n\n");

        // Providers
        for p in &self.providers {
            content.push_str("[provider:");
            content.push_str(&p.name);
            content.push_str("]\n");

            content.push_str("base_url=");
            content.push_str(&p.base_url);
            content.push('\n');

            content.push_str("api_type=");
            content.push_str(p.api_type.as_str());
            content.push('\n');

            if let Some(ref key) = p.api_key {
                content.push_str("api_key=");
                content.push_str(key);
                content.push('\n');
            }

            content.push('\n');
        }

        content
    }

    /// Get the current provider configuration
    pub fn get_current_provider(&self) -> Option<&Provider> {
        self.providers.iter().find(|p| p.name == self.current_provider)
    }

    /// Get a provider by name
    pub fn get_provider(&self, name: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.name == name)
    }

    /// Add or update a provider
    #[allow(dead_code)]
    pub fn set_provider(&mut self, provider: Provider) {
        if let Some(existing) = self.providers.iter_mut().find(|p| p.name == provider.name) {
            *existing = provider;
        } else {
            self.providers.push(provider);
        }
    }

    /// Remove a provider by name
    #[allow(dead_code)]
    pub fn remove_provider(&mut self, name: &str) -> bool {
        let initial_len = self.providers.len();
        self.providers.retain(|p| p.name != name);
        self.providers.len() < initial_len
    }
}
