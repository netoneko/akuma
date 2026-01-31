//! Provider API module for Meow-chan
//!
//! Handles communication with different AI provider APIs (Ollama, OpenAI-compatible)

use crate::config::{ApiType, Provider};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Result of listing models from a provider
#[derive(Debug)]
pub struct ModelInfo {
    pub name: String,
    pub size: Option<u64>,
    pub parameter_size: Option<String>,
}

/// Error type for provider operations
#[derive(Debug)]
pub enum ProviderError {
    ConnectionFailed(String),
    RequestFailed(String),
    ParseError(String),
    HttpsNotSupported,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::ConnectionFailed(msg) => write!(f, "Connection failed: {}", msg),
            ProviderError::RequestFailed(msg) => write!(f, "Request failed: {}", msg),
            ProviderError::ParseError(msg) => write!(f, "Parse error: {}", msg),
            ProviderError::HttpsNotSupported => write!(f, "HTTPS not supported in local mode"),
        }
    }
}

/// List available models from a provider
pub fn list_models(provider: &Provider) -> Result<Vec<ModelInfo>, ProviderError> {
    match provider.api_type {
        ApiType::Ollama => list_ollama_models(provider),
        ApiType::OpenAI => list_openai_models(provider),
    }
}

/// List models from Ollama API (GET /api/tags)
fn list_ollama_models(provider: &Provider) -> Result<Vec<ModelInfo>, ProviderError> {
    if provider.is_https() {
        return Err(ProviderError::HttpsNotSupported);
    }

    let (host, port) = provider.host_port()
        .ok_or_else(|| ProviderError::ConnectionFailed("Invalid URL".to_string()))?;

    let addr = format!("{}:{}", host, port);
    let mut stream = TcpStream::connect(&addr)
        .map_err(|e| ProviderError::ConnectionFailed(e.to_string()))?;

    stream.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| ProviderError::ConnectionFailed(e.to_string()))?;

    // Send GET request
    let request = format!(
        "GET /api/tags HTTP/1.0\r\n\
         Host: {}:{}\r\n\
         Connection: close\r\n\
         \r\n",
        host, port
    );

    stream.write_all(request.as_bytes())
        .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

    // Read response
    let mut response = Vec::new();
    stream.read_to_end(&mut response)
        .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

    let response_str = String::from_utf8_lossy(&response);
    
    // Find body (after \r\n\r\n)
    let body = response_str
        .find("\r\n\r\n")
        .map(|pos| &response_str[pos + 4..])
        .ok_or_else(|| ProviderError::ParseError("Invalid HTTP response".to_string()))?;

    // Parse JSON response
    parse_ollama_models(body)
}

/// Parse Ollama /api/tags response
fn parse_ollama_models(json: &str) -> Result<Vec<ModelInfo>, ProviderError> {
    let mut models = Vec::new();
    
    // Simple JSON parsing - look for "models" array
    // Format: {"models":[{"name":"...", "size":..., "details":{"parameter_size":"..."}}]}
    
    let models_start = json.find("\"models\"")
        .ok_or_else(|| ProviderError::ParseError("No models field found".to_string()))?;
    
    let json = &json[models_start..];
    let array_start = json.find('[')
        .ok_or_else(|| ProviderError::ParseError("No models array found".to_string()))?;
    
    let json = &json[array_start..];
    
    // Find each model object
    let mut depth = 0;
    let mut in_string = false;
    let mut escape_next = false;
    let mut obj_start = None;
    
    for (i, c) in json.chars().enumerate() {
        if escape_next {
            escape_next = false;
            continue;
        }
        
        match c {
            '\\' if in_string => escape_next = true,
            '"' => in_string = !in_string,
            '{' if !in_string => {
                if depth == 0 {
                    obj_start = Some(i);
                }
                depth += 1;
            }
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    if let Some(start) = obj_start {
                        let obj = &json[start..=i];
                        if let Some(model) = parse_model_object(obj) {
                            models.push(model);
                        }
                    }
                    obj_start = None;
                }
            }
            ']' if !in_string && depth == 0 => break,
            _ => {}
        }
    }
    
    Ok(models)
}

/// Parse a single model object from JSON
fn parse_model_object(json: &str) -> Option<ModelInfo> {
    let name = extract_json_string(json, "name")?;
    let size = extract_json_number(json, "size");
    let parameter_size = extract_json_string(json, "parameter_size");
    
    Some(ModelInfo {
        name,
        size,
        parameter_size,
    })
}

/// List models from OpenAI-compatible API (GET /v1/models)
fn list_openai_models(provider: &Provider) -> Result<Vec<ModelInfo>, ProviderError> {
    if provider.is_https() {
        return Err(ProviderError::HttpsNotSupported);
    }

    let (host, port) = provider.host_port()
        .ok_or_else(|| ProviderError::ConnectionFailed("Invalid URL".to_string()))?;

    let addr = format!("{}:{}", host, port);
    let mut stream = TcpStream::connect(&addr)
        .map_err(|e| ProviderError::ConnectionFailed(e.to_string()))?;

    stream.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| ProviderError::ConnectionFailed(e.to_string()))?;

    // Build request with optional API key
    let auth_header = match &provider.api_key {
        Some(key) => format!("Authorization: Bearer {}\r\n", key),
        None => String::new(),
    };

    let request = format!(
        "GET /v1/models HTTP/1.0\r\n\
         Host: {}:{}\r\n\
         {}Connection: close\r\n\
         \r\n",
        host, port, auth_header
    );

    stream.write_all(request.as_bytes())
        .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

    // Read response
    let mut response = Vec::new();
    stream.read_to_end(&mut response)
        .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

    let response_str = String::from_utf8_lossy(&response);
    
    // Check for HTTP errors
    if response_str.contains("401") || response_str.contains("Unauthorized") {
        return Err(ProviderError::RequestFailed("Unauthorized - check API key".to_string()));
    }
    
    // Find body
    let body = response_str
        .find("\r\n\r\n")
        .map(|pos| &response_str[pos + 4..])
        .ok_or_else(|| ProviderError::ParseError("Invalid HTTP response".to_string()))?;

    parse_openai_models(body)
}

/// Parse OpenAI /v1/models response
fn parse_openai_models(json: &str) -> Result<Vec<ModelInfo>, ProviderError> {
    let mut models = Vec::new();
    
    // Format: {"data":[{"id":"model-name", ...}]}
    let data_start = json.find("\"data\"")
        .ok_or_else(|| ProviderError::ParseError("No data field found".to_string()))?;
    
    let json = &json[data_start..];
    let array_start = json.find('[')
        .ok_or_else(|| ProviderError::ParseError("No data array found".to_string()))?;
    
    let json = &json[array_start..];
    
    // Find each model object
    let mut depth = 0;
    let mut in_string = false;
    let mut escape_next = false;
    let mut obj_start = None;
    
    for (i, c) in json.chars().enumerate() {
        if escape_next {
            escape_next = false;
            continue;
        }
        
        match c {
            '\\' if in_string => escape_next = true,
            '"' => in_string = !in_string,
            '{' if !in_string => {
                if depth == 0 {
                    obj_start = Some(i);
                }
                depth += 1;
            }
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    if let Some(start) = obj_start {
                        let obj = &json[start..=i];
                        // OpenAI uses "id" instead of "name"
                        if let Some(id) = extract_json_string(obj, "id") {
                            models.push(ModelInfo {
                                name: id,
                                size: None,
                                parameter_size: None,
                            });
                        }
                    }
                    obj_start = None;
                }
            }
            ']' if !in_string && depth == 0 => break,
            _ => {}
        }
    }
    
    Ok(models)
}

/// Extract a string value from JSON by key
fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\"", key);
    let start = json.find(&pattern)?;
    
    let after_key = &json[start + pattern.len()..];
    let colon_pos = after_key.find(':')?;
    let after_colon = &after_key[colon_pos + 1..];
    
    let trimmed = after_colon.trim_start();
    if !trimmed.starts_with('"') {
        return None;
    }
    
    let rest = &trimmed[1..];
    let mut result = String::new();
    let mut chars = rest.chars().peekable();
    
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => {
                if let Some(&next) = chars.peek() {
                    chars.next();
                    match next {
                        'n' => result.push('\n'),
                        'r' => result.push('\r'),
                        't' => result.push('\t'),
                        '"' => result.push('"'),
                        '\\' => result.push('\\'),
                        _ => {
                            result.push('\\');
                            result.push(next);
                        }
                    }
                }
            }
            _ => result.push(c),
        }
    }
    
    Some(result)
}

/// Extract a number value from JSON by key
fn extract_json_number(json: &str, key: &str) -> Option<u64> {
    let pattern = format!("\"{}\"", key);
    let start = json.find(&pattern)?;
    
    let after_key = &json[start + pattern.len()..];
    let colon_pos = after_key.find(':')?;
    let after_colon = &after_key[colon_pos + 1..];
    
    let trimmed = after_colon.trim_start();
    let end = trimmed.find(|c: char| !c.is_ascii_digit()).unwrap_or(trimmed.len());
    
    trimmed[..end].parse().ok()
}

/// Test connection to a provider
pub fn test_connection(provider: &Provider) -> Result<(), ProviderError> {
    if provider.is_https() {
        return Err(ProviderError::HttpsNotSupported);
    }

    let (host, port) = provider.host_port()
        .ok_or_else(|| ProviderError::ConnectionFailed("Invalid URL".to_string()))?;

    let addr = format!("{}:{}", host, port);
    let stream = TcpStream::connect_timeout(
        &addr.parse().map_err(|e| ProviderError::ConnectionFailed(format!("{}", e)))?,
        Duration::from_secs(5)
    ).map_err(|e| ProviderError::ConnectionFailed(e.to_string()))?;

    drop(stream);
    Ok(())
}
