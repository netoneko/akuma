//! Provider API module for Meow
//!
//! Handles communication with different AI provider APIs (Ollama, OpenAI-compatible)

use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;

use libakuma::net::{TcpStream, resolve};
use libakuma::sleep_ms;
use libakuma_tls::{TlsStream, TcpTransport, TLS_RECORD_SIZE};

use crate::config::{ApiType, Provider};

/// Result of listing models from a provider
#[derive(Debug)]
#[allow(dead_code)]
pub struct ModelInfo {
    pub name: String,
    pub size: Option<u64>,
    pub parameter_size: Option<String>,
}

/// Error type for provider operations
#[derive(Debug)]
#[allow(dead_code)]
pub enum ProviderError {
    ConnectionFailed(String),
    RequestFailed(String),
    ParseError(String),
}

/// Connect to a provider (HTTP only - for HTTPS use connect_tls)
pub fn connect(provider: &Provider) -> Result<TcpStream, ProviderError> {
    let (host, port) = provider.host_port()
        .ok_or_else(|| ProviderError::ConnectionFailed(String::from("Invalid URL")))?;

    let ip = resolve(&host).map_err(|_| {
        ProviderError::ConnectionFailed(format!("DNS resolution failed for: {}", host))
    })?;

    let addr_str = format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], port);

    TcpStream::connect(&addr_str).map_err(|_| {
        ProviderError::ConnectionFailed(format!("Connection failed to: {}", addr_str))
    })
}

/// Connect to a provider and establish TLS
fn connect_tls(provider: &Provider) -> Result<TcpStream, ProviderError> {
    let (host, port) = provider.host_port()
        .ok_or_else(|| ProviderError::ConnectionFailed(String::from("Invalid URL")))?;

    let ip = resolve(&host).map_err(|_| {
        ProviderError::ConnectionFailed(format!("DNS resolution failed for: {}", host))
    })?;

    let addr_str = format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], port);

    TcpStream::connect(&addr_str).map_err(|_| {
        ProviderError::ConnectionFailed(format!("Connection failed to: {}", addr_str))
    })
}

/// Read full response from stream (with timeout handling)
fn read_response(stream: &TcpStream) -> Result<String, ProviderError> {
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    let mut retries = 0;

    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                response.extend_from_slice(&buf[..n]);
                retries = 0;
                if response.len() > 256 * 1024 {
                    break; // Limit response size
                }
            }
            Err(e) => {
                if e.kind == libakuma::net::ErrorKind::WouldBlock
                    || e.kind == libakuma::net::ErrorKind::TimedOut
                {
                    retries += 1;
                    if retries > 100 {
                        break; // Timeout
                    }
                    sleep_ms(10);
                    continue;
                }
                break;
            }
        }
    }

    Ok(String::from_utf8_lossy(&response).into_owned())
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
    let (host, port) = provider.host_port()
        .ok_or_else(|| ProviderError::ConnectionFailed(String::from("Invalid URL")))?;

    let stream = connect(provider)?;

    // Send GET request
    let request = format!(
        "GET /api/tags HTTP/1.0\r\n\
         Host: {}:{}\r\n\
         Connection: close\r\n\
         \r\n",
        host, port
    );

    stream.write_all(request.as_bytes())
        .map_err(|_| ProviderError::RequestFailed(String::from("Write failed")))?;

    // Read response
    let response_str = read_response(&stream)?;

    // Find body (after \r\n\r\n)
    let body = response_str
        .find("\r\n\r\n")
        .map(|pos| &response_str[pos + 4..])
        .ok_or_else(|| ProviderError::ParseError(String::from("Invalid HTTP response")))?;

    // Parse JSON response
    parse_ollama_models(body)
}

/// Parse Ollama /api/tags response
fn parse_ollama_models(json: &str) -> Result<Vec<ModelInfo>, ProviderError> {
    let mut models = Vec::new();

    // Simple JSON parsing - look for "models" array
    let models_start = json.find("\"models\"")
        .ok_or_else(|| ProviderError::ParseError(String::from("No models field found")))?;

    let json = &json[models_start..];
    let array_start = json.find('[')
        .ok_or_else(|| ProviderError::ParseError(String::from("No models array found")))?;

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
    let (host, _port) = provider.host_port()
        .ok_or_else(|| ProviderError::ConnectionFailed(String::from("Invalid URL")))?;

    // Build request with optional API key
    let auth_header = match &provider.api_key {
        Some(key) => format!("Authorization: Bearer {}\r\n", key),
        None => String::new(),
    };

    // Use base_path from URL if provided
    let base = provider.base_path();
    let path = if base.is_empty() || base == "/" {
        String::from("/v1/models")
    } else if base.ends_with("/v1") {
        format!("{}/models", base)
    } else {
        format!("{}/models", base.trim_end_matches('/'))
    };

    let request = format!(
        "GET {} HTTP/1.0\r\n\
         Host: {}\r\n\
         {}Connection: close\r\n\
         \r\n",
        path, host, auth_header
    );

    let response_str = if provider.is_https() {
        // Use TLS for HTTPS
        let stream = connect_tls(provider)?;
        let transport = TcpTransport::new(stream);

        // Allocate TLS buffers
        let mut read_buf = alloc::vec![0u8; TLS_RECORD_SIZE];
        let mut write_buf = alloc::vec![0u8; TLS_RECORD_SIZE];

        let mut tls = TlsStream::connect(transport, &host, &mut read_buf, &mut write_buf)
            .map_err(|e| ProviderError::ConnectionFailed(format!("TLS error: {:?}", e)))?;

        tls.write_all(request.as_bytes())
            .map_err(|_| ProviderError::RequestFailed(String::from("TLS write failed")))?;
        tls.flush()
            .map_err(|_| ProviderError::RequestFailed(String::from("TLS flush failed")))?;

        read_response_tls(&mut tls)?
    } else {
        // Plain HTTP
        let stream = connect(provider)?;
        stream.write_all(request.as_bytes())
            .map_err(|_| ProviderError::RequestFailed(String::from("Write failed")))?;
        read_response(&stream)?
    };

    // Check for HTTP errors
    if response_str.contains("401") || response_str.contains("Unauthorized") {
        return Err(ProviderError::RequestFailed(String::from("Unauthorized - check API key")));
    }

    // Find body
    let body = response_str
        .find("\r\n\r\n")
        .map(|pos| &response_str[pos + 4..])
        .ok_or_else(|| ProviderError::ParseError(String::from("Invalid HTTP response")))?;

    parse_openai_models(body)
}

/// Read response from TLS stream
fn read_response_tls(tls: &mut TlsStream<'_>) -> Result<String, ProviderError> {
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];

    loop {
        match tls.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                response.extend_from_slice(&buf[..n]);
                if response.len() > 256 * 1024 {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    Ok(String::from_utf8_lossy(&response).into_owned())
}

/// Parse OpenAI /v1/models response
fn parse_openai_models(json: &str) -> Result<Vec<ModelInfo>, ProviderError> {
    let mut models = Vec::new();

    // Format: {"data":[{"id":"model-name", ...}]}
    let data_start = json.find("\"data\"")
        .ok_or_else(|| ProviderError::ParseError(String::from("No data field found")))?;

    let json = &json[data_start..];
    let array_start = json.find('[')
        .ok_or_else(|| ProviderError::ParseError(String::from("No data array found")))?;

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
#[allow(dead_code)]
pub fn test_connection(provider: &Provider) -> Result<(), ProviderError> {
    let _ = connect(provider)?;
    Ok(())
}

/// Query Ollama for model information including context window size
pub fn query_model_info(model: &str, provider: &Provider) -> Option<usize> {
    if provider.api_type != ApiType::Ollama {
        return None;
    }

    let (host, port) = provider.host_port()?;
    let stream = connect(provider).ok()?;

    let body = format!("{{\"model\":\"{}\"}}", model);
    let request = format!(
        "POST /api/show HTTP/1.0\r\n\
         Host: {}:{}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        host, port, body.len(), body
    );

    stream.write_all(request.as_bytes()).ok()?;

    let response_str = read_response(&stream).ok()?;

    // Look for "num_ctx" in the response
    if let Some(pos) = response_str.find("\"num_ctx\"") {
        let after = &response_str[pos + 9..];
        let num_start = after.find(|c: char| c.is_ascii_digit())?;
        let rest = &after[num_start..];
        let num_end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        let num_str = &rest[..num_end];
        return num_str.parse().ok();
    }

    None
}
