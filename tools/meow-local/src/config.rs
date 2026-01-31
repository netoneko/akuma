//! Configuration module for Meow-chan
//!
//! Handles loading and saving configuration from ~/.config/meow/config.toml

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// API type for the provider
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ApiType {
    Ollama,
    OpenAI,
}

impl Default for ApiType {
    fn default() -> Self {
        ApiType::Ollama
    }
}

/// A configured AI provider
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    pub api_type: ApiType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl Provider {
    /// Create a new Ollama provider with default settings
    pub fn ollama_default() -> Self {
        Provider {
            name: String::from("ollama"),
            base_url: String::from("http://localhost:11434"),
            api_type: ApiType::Ollama,
            api_key: None,
        }
    }

    /// Create a new OpenAI provider
    pub fn openai(api_key: String) -> Self {
        Provider {
            name: String::from("openai"),
            base_url: String::from("https://api.openai.com"),
            api_type: ApiType::OpenAI,
            api_key: Some(api_key),
        }
    }

    /// Get the host and port from the base_url
    pub fn host_port(&self) -> Option<(String, u16)> {
        let url = self.base_url.trim_start_matches("http://").trim_start_matches("https://");
        let (host_port, _path) = url.split_once('/').unwrap_or((url, ""));
        
        if let Some((host, port_str)) = host_port.rsplit_once(':') {
            if let Ok(port) = port_str.parse::<u16>() {
                return Some((host.to_string(), port));
            }
        }
        
        // Default ports
        let default_port = if self.base_url.starts_with("https://") { 443 } else { 80 };
        Some((host_port.to_string(), default_port))
    }

    /// Check if this provider uses HTTPS
    pub fn is_https(&self) -> bool {
        self.base_url.starts_with("https://")
    }

    /// Get the base path from the URL (e.g., "/openai/v1" from "https://api.groq.com/openai/v1")
    pub fn base_path(&self) -> &str {
        let url = self.base_url.trim_start_matches("http://").trim_start_matches("https://");
        match url.find('/') {
            Some(pos) => &url[pos..],
            None => "",
        }
    }
}

/// Main configuration structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub current_provider: String,
    pub current_model: String,
    #[serde(default)]
    pub providers: Vec<Provider>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            current_provider: String::from("ollama"),
            current_model: String::from("gemma3:27b"),
            providers: vec![Provider::ollama_default()],
        }
    }
}

impl Config {
    /// Get the config file path (~/.config/meow/config.toml)
    pub fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|mut p| {
            p.push("meow");
            p.push("config.toml");
            p
        })
    }

    /// Load configuration from disk
    /// Returns default config if file doesn't exist
    pub fn load() -> Self {
        let path = match Self::config_path() {
            Some(p) => p,
            None => return Self::default(),
        };

        if !path.exists() {
            return Self::default();
        }

        match fs::read_to_string(&path) {
            Ok(contents) => match toml::from_str(&contents) {
                Ok(config) => config,
                Err(e) => {
                    eprintln!("Warning: Failed to parse config file: {}", e);
                    Self::default()
                }
            },
            Err(e) => {
                eprintln!("Warning: Failed to read config file: {}", e);
                Self::default()
            }
        }
    }

    /// Save configuration to disk
    pub fn save(&self) -> Result<(), String> {
        let path = Self::config_path()
            .ok_or_else(|| String::from("Could not determine config directory"))?;

        // Create parent directory if needed
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config directory: {}", e))?;
        }

        let contents = toml::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize config: {}", e))?;

        fs::write(&path, contents)
            .map_err(|e| format!("Failed to write config file: {}", e))?;

        Ok(())
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
    pub fn set_provider(&mut self, provider: Provider) {
        if let Some(existing) = self.providers.iter_mut().find(|p| p.name == provider.name) {
            *existing = provider;
        } else {
            self.providers.push(provider);
        }
    }

    /// Remove a provider by name
    pub fn remove_provider(&mut self, name: &str) -> bool {
        let initial_len = self.providers.len();
        self.providers.retain(|p| p.name != name);
        self.providers.len() < initial_len
    }

    /// Set the current provider and model
    pub fn set_current(&mut self, provider_name: &str, model: &str) {
        self.current_provider = provider_name.to_string();
        self.current_model = model.to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.current_provider, "ollama");
        assert_eq!(config.current_model, "gemma3:27b");
        assert_eq!(config.providers.len(), 1);
    }

    #[test]
    fn test_provider_host_port() {
        let provider = Provider::ollama_default();
        let (host, port) = provider.host_port().unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 11434);
    }

    #[test]
    fn test_serialization() {
        let config = Config::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(toml_str.contains("current_provider"));
        assert!(toml_str.contains("ollama"));
    }
}
