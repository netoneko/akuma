//! Tool execution module for Meow-chan
//!
//! Implements file system, network, and shell tools that the LLM can invoke via JSON commands.
//! Tools are executed using libakuma syscalls.

use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;

use libakuma::{
    open, close, read_fd, write_fd, fstat, mkdir, read_dir,
    open_flags, spawn, waitpid,
};
use libakuma::net::{TcpStream, resolve};

use crate::code_search;

// ============================================================================
// Working Directory (for scratch and other tools)
// ============================================================================

/// Current working directory for git operations
static mut WORKING_DIR: Option<String> = None;

/// Get the current working directory (from process cwd or override)
pub fn get_working_dir() -> String {
    unsafe {
        match &WORKING_DIR {
            Some(dir) => dir.clone(),
            None => String::from(libakuma::getcwd()),
        }
    }
}

/// Set the current working directory
pub fn set_working_dir(path: &str) {
    // Normalize path - ensure it starts with /
    let normalized = if path.starts_with('/') {
        String::from(path)
    } else {
        format!("/{}", path)
    };
    
    // Remove trailing slash unless it's root
    let normalized = if normalized.len() > 1 && normalized.ends_with('/') {
        String::from(&normalized[..normalized.len()-1])
    } else {
        normalized
    };
    
    unsafe {
        WORKING_DIR = Some(normalized);
    }
}

/// Resolve a path relative to the working directory
/// Returns None if the path escapes the sandbox (tries to go above working dir)
fn resolve_path(path: &str) -> Option<String> {
    let cwd = get_working_dir();
    
    // If path is absolute, check if it's within the sandbox
    if path.starts_with('/') {
        // Path must be within the working directory (or be the working dir itself)
        if path == cwd || path.starts_with(&format!("{}/", cwd)) || cwd == "/" {
            return Some(String::from(path));
        } else {
            return None; // Trying to escape sandbox
        }
    }
    
    // Handle relative paths
    let mut parts: Vec<&str> = if cwd == "/" {
        Vec::new()
    } else {
        cwd[1..].split('/').collect()
    };
    
    for component in path.split('/') {
        match component {
            "" | "." => continue,
            ".." => {
                if parts.is_empty() {
                    return None; // Trying to go above sandbox root
                }
                parts.pop();
            }
            name => parts.push(name),
        }
    }
    
    if parts.is_empty() {
        Some(String::from("/"))
    } else {
        Some(format!("/{}", parts.join("/")))
    }
}

/// Resolve path or return error
fn resolve_path_or_err(path: &str) -> Result<String, ToolResult> {
    match resolve_path(path) {
        Some(p) => Ok(p),
        None => Err(ToolResult::err(&format!(
            "Access denied: '{}' is outside the working directory '{}'",
            path, get_working_dir()
        ))),
    }
}

/// Result of a tool execution
pub struct ToolResult {
    pub success: bool,
    pub output: String,
}

impl ToolResult {
    pub fn ok(output: String) -> Self {
        Self { success: true, output }
    }
    
    pub fn err(message: &str) -> Self {
        Self { success: false, output: String::from(message) }
    }
}

/// Parse and execute a tool command from JSON
/// 
/// Expected format:
/// ```json
/// {
///   "command": {
///     "tool": "ToolName",
///     "args": { ... }
///   }
/// }
/// ```
pub fn execute_tool_command(json: &str) -> Option<ToolResult> {
    // Extract tool name
    let tool_name = extract_string_field(json, "tool")?;
    
    match tool_name.as_str() {
        "FileRead" => {
            let filename = extract_string_field(json, "filename")?;
            Some(tool_file_read(&filename))
        }
        "FileWrite" => {
            let filename = extract_string_field(json, "filename")?;
            let content = extract_string_field(json, "content").unwrap_or_default();
            Some(tool_file_write(&filename, &content))
        }
        "FileAppend" => {
            let filename = extract_string_field(json, "filename")?;
            let content = extract_string_field(json, "content")?;
            Some(tool_file_append(&filename, &content))
        }
        "FileExists" => {
            let filename = extract_string_field(json, "filename")?;
            Some(tool_file_exists(&filename))
        }
        "FileList" => {
            let path = extract_string_field(json, "path").unwrap_or_else(|| String::from("/"));
            Some(tool_file_list(&path))
        }
        "FileDelete" => {
            let filename = extract_string_field(json, "filename")?;
            Some(tool_file_delete(&filename))
        }
        "FolderCreate" => {
            let path = extract_string_field(json, "path")?;
            Some(tool_folder_create(&path))
        }
        "FileRename" => {
            let source = extract_string_field(json, "source_filename")?;
            let dest = extract_string_field(json, "destination_filename")?;
            Some(tool_file_rename(&source, &dest))
        }
        "FileCopy" => {
            let source = extract_string_field(json, "source")?;
            let dest = extract_string_field(json, "destination")?;
            Some(tool_file_copy(&source, &dest))
        }
        "FileMove" => {
            let source = extract_string_field(json, "source")?;
            let dest = extract_string_field(json, "destination")?;
            Some(tool_file_move(&source, &dest))
        }
        "HttpFetch" => {
            let url = extract_string_field(json, "url")?;
            Some(tool_http_fetch(&url))
        }
        "GitClone" => {
            let url = extract_string_field(json, "url")?;
            Some(tool_git_clone(&url))
        }
        "GitPull" => {
            Some(tool_git_pull())
        }
        "GitPush" => {
            // Check for force flag - ALWAYS DENIED
            let force = extract_string_field(json, "force")
                .map(|s| s == "true")
                .unwrap_or(false);
            Some(tool_git_push(force))
        }
        "GitStatus" => {
            Some(tool_git_status())
        }
        "GitBranch" => {
            let name = extract_string_field(json, "name");
            let delete = extract_string_field(json, "delete")
                .map(|s| s == "true")
                .unwrap_or(false);
            Some(tool_git_branch(name.as_deref(), delete))
        }
        "GitFetch" => {
            Some(tool_git_fetch())
        }
        "GitAdd" => {
            let path = extract_string_field(json, "path").unwrap_or_else(|| String::from("."));
            Some(tool_git_add(&path))
        }
        "GitCommit" => {
            let message = extract_string_field(json, "message")?;
            let amend = extract_string_field(json, "amend")
                .map(|s| s == "true")
                .unwrap_or(false);
            Some(tool_git_commit(&message, amend))
        }
        "GitCheckout" => {
            let branch = extract_string_field(json, "branch")?;
            Some(tool_git_checkout(&branch))
        }
        "GitConfig" => {
            let key = extract_string_field(json, "key")?;
            let value = extract_string_field(json, "value");
            Some(tool_git_config(&key, value.as_deref()))
        }
        "GitLog" => {
            let count = extract_number_field(json, "count");
            let oneline = extract_string_field(json, "oneline")
                .map(|s| s == "true")
                .unwrap_or(false);
            Some(tool_git_log(count, oneline))
        }
        "GitTag" => {
            let name = extract_string_field(json, "name");
            let delete = extract_string_field(json, "delete")
                .map(|s| s == "true")
                .unwrap_or(false);
            Some(tool_git_tag(name.as_deref(), delete))
        }
        "FileReadLines" => {
            let filename = extract_string_field(json, "filename")?;
            let start = extract_number_field(json, "start").unwrap_or(1);
            let end = extract_number_field(json, "end").unwrap_or(start + 50);
            Some(tool_file_read_lines(&filename, start, end))
        }
        "CodeSearch" => {
            let pattern = extract_string_field(json, "pattern")?;
            let path = extract_string_field(json, "path").unwrap_or_else(|| String::from("."));
            let context = extract_number_field(json, "context").unwrap_or(2);
            Some(tool_code_search(&pattern, &path, context))
        }
        "FileEdit" => {
            let filename = extract_string_field(json, "filename")?;
            let old_text = extract_string_field(json, "old_text")?;
            let new_text = extract_string_field(json, "new_text")?;
            Some(tool_file_edit(&filename, &old_text, &new_text))
        }
        "Shell" => {
            let cmd = extract_string_field(json, "cmd")?;
            Some(tool_shell(&cmd))
        }
        "Cd" => {
            let path = extract_string_field(json, "path")?;
            Some(tool_cd(&path))
        }
        "Pwd" => {
            Some(tool_pwd())
        }
        _ => None,
    }
}

/// Try to find and execute a tool command in the LLM's response
/// Returns (remaining_text, Some(result)) if a tool was found and executed
/// Returns (original_text, None) if no tool command was found
pub fn find_and_execute_tool(response: &str) -> (String, Option<ToolResult>) {
    // Look for JSON code block with command
    if let Some(start) = response.find("```json") {
        if let Some(end) = response[start..].find("```\n").or_else(|| response[start..].rfind("```")) {
            let json_start = start + 7; // Skip ```json
            let json_end = start + end;
            
            if json_start < json_end && json_end <= response.len() {
                let json_block = response[json_start..json_end].trim();
                
                // Check if this looks like a command
                if json_block.contains("\"command\"") && json_block.contains("\"tool\"") {
                    if let Some(result) = execute_tool_command(json_block) {
                        // Return text before the JSON block
                        let before = response[..start].trim();
                        return (String::from(before), Some(result));
                    }
                }
            }
        }
    }
    
    // Also try inline JSON (without code blocks)
    if let Some(start) = response.find("{\"command\"") {
        // Find matching closing brace
        let mut depth = 0;
        let mut end = start;
        for (i, c) in response[start..].chars().enumerate() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        
        if end > start {
            let json_block = &response[start..end];
            if let Some(result) = execute_tool_command(json_block) {
                let before = response[..start].trim();
                return (String::from(before), Some(result));
            }
        }
    }
    
    (String::from(response), None)
}

// ============================================================================
// Tool Implementations
// ============================================================================

// Keep file operations reasonable to avoid OOM
const MAX_FILE_SIZE: usize = 32 * 1024; // 32KB max

fn tool_file_read(filename: &str) -> ToolResult {
    let resolved = match resolve_path_or_err(filename) {
        Ok(p) => p,
        Err(e) => return e,
    };
    
    let fd = open(&resolved, open_flags::O_RDONLY);
    if fd < 0 {
        return ToolResult::err(&format!("Failed to open file: {}", filename));
    }
    
    // Get file size
    let stat = match fstat(fd) {
        Ok(s) => s,
        Err(_) => {
            close(fd);
            return ToolResult::err("Failed to get file info");
        }
    };
    
    let size = stat.st_size as usize;
    if size > MAX_FILE_SIZE {
        close(fd);
        return ToolResult::err("File too large (max 32KB)");
    }
    
    let mut buf = alloc::vec![0u8; size];
    let bytes_read = read_fd(fd, &mut buf);
    close(fd);
    
    if bytes_read < 0 {
        return ToolResult::err("Failed to read file");
    }
    
    match core::str::from_utf8(&buf[..bytes_read as usize]) {
        Ok(content) => ToolResult::ok(format!("Contents of '{}':\n```\n{}\n```", filename, content)),
        Err(_) => ToolResult::err("File contains non-UTF8 data"),
    }
}

fn tool_file_write(filename: &str, content: &str) -> ToolResult {
    let resolved = match resolve_path_or_err(filename) {
        Ok(p) => p,
        Err(e) => return e,
    };
    
    let fd = open(&resolved, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd < 0 {
        return ToolResult::err(&format!("Failed to create file: {}", filename));
    }
    
    let bytes_written = write_fd(fd, content.as_bytes());
    close(fd);
    
    if bytes_written < 0 {
        return ToolResult::err("Failed to write to file");
    }
    
    ToolResult::ok(format!("Successfully wrote {} bytes to '{}'", bytes_written, filename))
}

fn tool_file_append(filename: &str, content: &str) -> ToolResult {
    let resolved = match resolve_path_or_err(filename) {
        Ok(p) => p,
        Err(e) => return e,
    };
    
    let fd = open(&resolved, open_flags::O_WRONLY | open_flags::O_APPEND);
    if fd < 0 {
        return ToolResult::err(&format!("Failed to open file for append: {}", filename));
    }
    
    let bytes_written = write_fd(fd, content.as_bytes());
    close(fd);
    
    if bytes_written < 0 {
        return ToolResult::err("Failed to append to file");
    }
    
    ToolResult::ok(format!("Successfully appended {} bytes to '{}'", bytes_written, filename))
}

fn tool_file_exists(filename: &str) -> ToolResult {
    let resolved = match resolve_path_or_err(filename) {
        Ok(p) => p,
        Err(e) => return e,
    };
    
    let fd = open(&resolved, open_flags::O_RDONLY);
    if fd >= 0 {
        close(fd);
        ToolResult::ok(format!("'{}' exists", filename))
    } else {
        ToolResult::ok(format!("'{}' does not exist", filename))
    }
}

fn tool_file_list(path: &str) -> ToolResult {
    let resolved = match resolve_path_or_err(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    
    match read_dir(&resolved) {
        Some(entries) => {
            let mut output = format!("Contents of '{}':\n", path);
            let mut count = 0;
            for entry in entries {
                let type_indicator = if entry.is_dir { "/" } else { "" };
                output.push_str(&format!("  {}{}\n", entry.name, type_indicator));
                count += 1;
            }
            if count == 0 {
                output.push_str("  (empty directory)\n");
            }
            ToolResult::ok(output)
        }
        None => ToolResult::err(&format!("Failed to list directory: {}", path)),
    }
}

fn tool_file_delete(filename: &str) -> ToolResult {
    let _resolved = match resolve_path_or_err(filename) {
        Ok(p) => p,
        Err(e) => return e,
    };
    // Note: libakuma doesn't have unlink syscall yet
    // For now, we'll return an error
    ToolResult::err(&format!("Delete not yet implemented for: {}", filename))
}

fn tool_folder_create(path: &str) -> ToolResult {
    let resolved = match resolve_path_or_err(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    
    let result = mkdir(&resolved);
    if result >= 0 {
        ToolResult::ok(format!("Successfully created directory: '{}'", path))
    } else {
        ToolResult::err(&format!("Failed to create directory: {}", path))
    }
}

/// Change the working directory for git operations
fn tool_cd(path: &str) -> ToolResult {
    // Resolve path relative to current working dir if not absolute
    let new_path = if path.starts_with('/') {
        String::from(path)
    } else if path == ".." {
        // Handle parent directory
        let current = get_working_dir();
        if current == "/" {
            String::from("/")
        } else if let Some(last_slash) = current.rfind('/') {
            if last_slash == 0 {
                String::from("/")
            } else {
                String::from(&current[..last_slash])
            }
        } else {
            String::from("/")
        }
    } else {
        let current = get_working_dir();
        if current == "/" {
            format!("/{}", path)
        } else {
            format!("{}/{}", current, path)
        }
    };
    
    // Use chdir syscall to update the process's cwd
    let result = libakuma::chdir(&new_path);
    if result == 0 {
        // Also update our local tracking variable
        set_working_dir(&new_path);
        ToolResult::ok(format!("Changed directory to: {}", new_path))
    } else if result == -2 {
        // ENOENT
        ToolResult::err(&format!("Directory not found: {}", new_path))
    } else {
        ToolResult::err(&format!("Failed to change directory: error {}", result))
    }
}

/// Print the current working directory
fn tool_pwd() -> ToolResult {
    ToolResult::ok(get_working_dir())
}

fn tool_file_rename(source: &str, dest: &str) -> ToolResult {
    let src_resolved = match resolve_path_or_err(source) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let dst_resolved = match resolve_path_or_err(dest) {
        Ok(p) => p,
        Err(e) => return e,
    };
    
    // Implement as copy + delete (when delete is available)
    // For now, just copy
    match tool_file_copy_internal(&src_resolved, &dst_resolved) {
        Ok(_) => ToolResult::ok(format!("Renamed '{}' to '{}' (note: original not deleted yet)", source, dest)),
        Err(e) => ToolResult::err(&e),
    }
}

fn tool_file_copy(source: &str, dest: &str) -> ToolResult {
    let src_resolved = match resolve_path_or_err(source) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let dst_resolved = match resolve_path_or_err(dest) {
        Ok(p) => p,
        Err(e) => return e,
    };
    
    match tool_file_copy_internal(&src_resolved, &dst_resolved) {
        Ok(msg) => ToolResult::ok(msg),
        Err(e) => ToolResult::err(&e),
    }
}

fn tool_file_copy_internal(source: &str, dest: &str) -> Result<String, String> {
    // Paths should already be resolved by callers
    // Read source file
    let src_fd = open(source, open_flags::O_RDONLY);
    if src_fd < 0 {
        return Err(format!("Failed to open source: {}", source));
    }
    
    let stat = match fstat(src_fd) {
        Ok(s) => s,
        Err(_) => {
            close(src_fd);
            return Err(String::from("Failed to get file info"));
        }
    };
    
    let size = stat.st_size as usize;
    if size > MAX_FILE_SIZE {
        close(src_fd);
        return Err(String::from("File too large (max 32KB)"));
    }
    
    let mut buf = alloc::vec![0u8; size];
    let bytes_read = read_fd(src_fd, &mut buf);
    close(src_fd);
    
    if bytes_read < 0 {
        return Err(String::from("Failed to read source file"));
    }
    
    // Write to destination
    let dst_fd = open(dest, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if dst_fd < 0 {
        return Err(format!("Failed to create destination: {}", dest));
    }
    
    let bytes_written = write_fd(dst_fd, &buf[..bytes_read as usize]);
    close(dst_fd);
    
    if bytes_written < 0 {
        return Err(String::from("Failed to write destination file"));
    }
    
    Ok(format!("Copied '{}' to '{}' ({} bytes)", source, dest, bytes_written))
}

fn tool_file_move(source: &str, dest: &str) -> ToolResult {
    let src_resolved = match resolve_path_or_err(source) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let dst_resolved = match resolve_path_or_err(dest) {
        Ok(p) => p,
        Err(e) => return e,
    };
    
    // Copy first
    match tool_file_copy_internal(&src_resolved, &dst_resolved) {
        Ok(_) => ToolResult::ok(format!("Moved '{}' to '{}' (note: source not deleted yet)", source, dest)),
        Err(e) => ToolResult::err(&e),
    }
}

// ============================================================================
// Network Tools
// ============================================================================

// Maximum response size for HTTP fetch (64KB)
const MAX_FETCH_SIZE: usize = 64 * 1024;

/// HTTP/HTTPS GET fetch tool
/// Supports both HTTP and HTTPS URLs using libakuma-tls
fn tool_http_fetch(url: &str) -> ToolResult {
    // Parse URL to check validity
    let parsed = match parse_http_url(url) {
        Some(p) => p,
        None => return ToolResult::err("Invalid URL format. Use: http(s)://host[:port]/path"),
    };

    if parsed.is_https {
        // Use libakuma-tls for HTTPS
        match libakuma_tls::https_fetch(url, true) {
            Ok(body) => {
                match core::str::from_utf8(&body) {
                    Ok(text) => {
                        let truncated = if body.len() >= MAX_FETCH_SIZE { " (truncated)" } else { "" };
                        ToolResult::ok(format!(
                            "Fetched {} ({} bytes{}):\n```\n{}\n```",
                            url, body.len(), truncated, text
                        ))
                    }
                    Err(_) => ToolResult::err("Response contains non-UTF8 data (binary content)"),
                }
            }
            Err(e) => ToolResult::err(&format!("HTTPS fetch failed: {:?}", e)),
        }
    } else {
        // Plain HTTP - use direct TCP
        let ip = match resolve(parsed.host) {
            Ok(ip) => ip,
            Err(_) => return ToolResult::err(&format!("DNS resolution failed for: {}", parsed.host)),
        };

        let addr_str = format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], parsed.port);
        let stream = match TcpStream::connect(&addr_str) {
            Ok(s) => s,
            Err(_) => return ToolResult::err(&format!("Connection failed to: {}", addr_str)),
        };

        // Build HTTP request
        let request = format!(
            "GET {} HTTP/1.0\r\n\
             Host: {}\r\n\
             User-Agent: meow/1.0 (Akuma)\r\n\
             Connection: close\r\n\
             \r\n",
            parsed.path,
            parsed.host
        );

        // Send request
        if stream.write_all(request.as_bytes()).is_err() {
            return ToolResult::err("Failed to send HTTP request");
        }

        // Read response with size limit
        let mut response = Vec::new();
        let mut buf = [0u8; 1024];

        loop {
            match stream.read(&mut buf) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    if response.len() + n > MAX_FETCH_SIZE {
                        let remaining = MAX_FETCH_SIZE - response.len();
                        response.extend_from_slice(&buf[..remaining]);
                        break;
                    }
                    response.extend_from_slice(&buf[..n]);
                }
                Err(e) => {
                    if e.kind == libakuma::net::ErrorKind::WouldBlock {
                        libakuma::sleep_ms(10);
                        continue;
                    }
                    break;
                }
            }
        }

        if response.is_empty() {
            return ToolResult::err("Empty response from server");
        }

        // Parse HTTP response
        let (status, body) = match parse_http_response(&response) {
            Some(r) => r,
            None => return ToolResult::err("Failed to parse HTTP response"),
        };

        if status < 200 || status >= 300 {
            return ToolResult::err(&format!("HTTP error: status {}", status));
        }

        // Convert body to string
        match core::str::from_utf8(body) {
            Ok(text) => {
                let truncated = if response.len() >= MAX_FETCH_SIZE { " (truncated)" } else { "" };
                ToolResult::ok(format!(
                    "Fetched {} ({} bytes{}):\n```\n{}\n```",
                    url, body.len(), truncated, text
                ))
            }
            Err(_) => ToolResult::err("Response contains non-UTF8 data (binary content)"),
        }
    }
}

/// Parsed HTTP URL
struct ParsedUrl<'a> {
    is_https: bool,
    host: &'a str,
    port: u16,
    path: &'a str,
}

/// Parse an HTTP(S) URL
fn parse_http_url(url: &str) -> Option<ParsedUrl<'_>> {
    let (is_https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return None;
    };
    
    let default_port = if is_https { 443 } else { 80 };
    
    // Split host:port from path
    let (host_port, path) = match rest.find('/') {
        Some(pos) => (&rest[..pos], &rest[pos..]),
        None => (rest, "/"),
    };
    
    // Parse host and port
    let (host, port) = match host_port.rfind(':') {
        Some(pos) => {
            let h = &host_port[..pos];
            let p = host_port[pos + 1..].parse::<u16>().ok()?;
            (h, p)
        }
        None => (host_port, default_port),
    };
    
    Some(ParsedUrl { is_https, host, port, path })
}

/// Parse HTTP response, returns (status_code, body_slice)
fn parse_http_response(data: &[u8]) -> Option<(u16, &[u8])> {
    // Find headers end
    let headers_end = find_headers_end(data)?;
    
    // Parse status line
    let header_str = core::str::from_utf8(&data[..headers_end]).ok()?;
    let first_line = header_str.lines().next()?;
    
    // Parse "HTTP/1.x STATUS MESSAGE"
    let mut parts = first_line.split_whitespace();
    let _version = parts.next()?;
    let status: u16 = parts.next()?.parse().ok()?;
    
    Some((status, &data[headers_end..]))
}

/// Find the end of HTTP headers (\r\n\r\n)
fn find_headers_end(data: &[u8]) -> Option<usize> {
    for i in 0..data.len().saturating_sub(3) {
        if &data[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 4);
        }
    }
    None
}

// ============================================================================
// Git Tools (via scratch binary using Shell tool)
// ============================================================================

/// Clone a Git repository
fn tool_git_clone(url: &str) -> ToolResult {
    tool_shell(&format!("scratch clone {}", url))
}

/// Pull from remote (fetch + checkout)
fn tool_git_pull() -> ToolResult {
    // First fetch
    let fetch_result = tool_shell("scratch fetch");
    if !fetch_result.success {
        return fetch_result;
    }
    
    // Note: actual merge/checkout after fetch not yet implemented in scratch
    ToolResult::ok(format!("{}\nNote: Pull fetched updates. Manual checkout may be needed.", fetch_result.output))
}

/// Fetch from remote
fn tool_git_fetch() -> ToolResult {
    tool_shell("scratch fetch")
}

/// Push to remote - FORCE PUSH IS ALWAYS DENIED
fn tool_git_push(force: bool) -> ToolResult {
    if force {
        // Immediately deny force push without even calling scratch
        return ToolResult::err("DENIED: Force push is permanently disabled. This cannot be bypassed.");
    }
    
    tool_shell("scratch push")
}

/// Get repository status
fn tool_git_status() -> ToolResult {
    tool_shell("scratch status")
}

/// List, create, or delete branches
fn tool_git_branch(name: Option<&str>, delete: bool) -> ToolResult {
    match (name, delete) {
        (None, _) => tool_shell("scratch branch"),
        (Some(n), true) => tool_shell(&format!("scratch branch -d {}", n)),
        (Some(n), false) => tool_shell(&format!("scratch branch {}", n)),
    }
}

/// Stage files for commit
fn tool_git_add(path: &str) -> ToolResult {
    tool_shell(&format!("scratch add {}", path))
}

/// Create a commit
fn tool_git_commit(message: &str, amend: bool) -> ToolResult {
    // Escape double quotes in message
    let escaped_message = message.replace('"', "\\\"");
    if amend {
        tool_shell(&format!("scratch commit --amend -m \"{}\"", escaped_message))
    } else {
        tool_shell(&format!("scratch commit -m \"{}\"", escaped_message))
    }
}

/// Checkout a branch
fn tool_git_checkout(branch: &str) -> ToolResult {
    tool_shell(&format!("scratch checkout {}", branch))
}

/// Get or set git config
fn tool_git_config(key: &str, value: Option<&str>) -> ToolResult {
    match value {
        Some(v) => {
            // Escape double quotes in value
            let escaped_value = v.replace('"', "\\\"");
            tool_shell(&format!("scratch config {} \"{}\"", key, escaped_value))
        }
        None => tool_shell(&format!("scratch config {}", key)),
    }
}

/// Show commit log
fn tool_git_log(count: Option<usize>, oneline: bool) -> ToolResult {
    let mut cmd = String::from("scratch log");
    if let Some(n) = count {
        cmd.push_str(&format!(" -n {}", n));
    }
    if oneline {
        cmd.push_str(" --oneline");
    }
    tool_shell(&cmd)
}

/// List, create, or delete tags
fn tool_git_tag(name: Option<&str>, delete: bool) -> ToolResult {
    match (name, delete) {
        (None, _) => tool_shell("scratch tag"),
        (Some(n), true) => tool_shell(&format!("scratch tag -d {}", n)),
        (Some(n), false) => tool_shell(&format!("scratch tag {}", n)),
    }
}

// ============================================================================
// JSON Parsing Helpers
// ============================================================================

/// Extract a string field from JSON (simple implementation)
fn extract_string_field(json: &str, field: &str) -> Option<String> {
    let pattern = format!("\"{}\"", field);
    let start = json.find(&pattern)?;

    let after_field = &json[start + pattern.len()..];
    let colon_pos = after_field.find(':')?;
    let after_colon = &after_field[colon_pos + 1..];

    let trimmed = after_colon.trim_start();

    if !trimmed.starts_with('"') {
        return None;
    }

    let value_start = 1;
    let rest = &trimmed[value_start..];

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
                        '/' => result.push('/'),
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

/// Extract a number field from JSON
fn extract_number_field(json: &str, field: &str) -> Option<usize> {
    let pattern = format!("\"{}\"", field);
    let start = json.find(&pattern)?;

    let after_field = &json[start + pattern.len()..];
    let colon_pos = after_field.find(':')?;
    let after_colon = &after_field[colon_pos + 1..];

    let trimmed = after_colon.trim_start();

    let num_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    if num_end == 0 {
        return None;
    }

    trimmed[..num_end].parse().ok()
}

// ============================================================================
// FileReadLines Tool
// ============================================================================

fn tool_file_read_lines(filename: &str, start: usize, end: usize) -> ToolResult {
    let resolved = match resolve_path_or_err(filename) {
        Ok(p) => p,
        Err(e) => return e,
    };
    
    let fd = open(&resolved, open_flags::O_RDONLY);
    if fd < 0 {
        return ToolResult::err(&format!("Failed to open file: {}", filename));
    }

    let stat = match fstat(fd) {
        Ok(s) => s,
        Err(_) => {
            close(fd);
            return ToolResult::err("Failed to get file info");
        }
    };

    let size = stat.st_size as usize;
    if size > MAX_FILE_SIZE * 4 {
        // Allow larger files for line reading
        close(fd);
        return ToolResult::err("File too large");
    }

    let mut buf = alloc::vec![0u8; size];
    let bytes_read = read_fd(fd, &mut buf);
    close(fd);

    if bytes_read <= 0 {
        return ToolResult::err("Failed to read file");
    }

    let content = match core::str::from_utf8(&buf[..bytes_read as usize]) {
        Ok(s) => s,
        Err(_) => return ToolResult::err("File contains non-UTF8 data"),
    };

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();
    let start_idx = start.saturating_sub(1); // Convert to 0-indexed
    let end_idx = end.min(total_lines);

    if start_idx >= total_lines {
        return ToolResult::err(&format!(
            "Start line {} is beyond file length ({} lines)",
            start, total_lines
        ));
    }

    let mut output = format!(
        "Lines {}-{} of '{}' ({} total lines):\n```\n",
        start, end_idx, filename, total_lines
    );

    for (idx, line) in lines[start_idx..end_idx].iter().enumerate() {
        let line_num = start_idx + idx + 1;
        output.push_str(&format!("{:>4}: {}\n", line_num, line));
    }
    output.push_str("```");

    ToolResult::ok(output)
}

// ============================================================================
// CodeSearch Tool
// ============================================================================

fn tool_code_search(pattern: &str, path: &str, context: usize) -> ToolResult {
    let resolved = match resolve_path_or_err(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    
    match code_search::search_to_string(pattern, &resolved, context) {
        Ok(results) => ToolResult::ok(results),
        Err(e) => ToolResult::err(&format!("Search failed: {}", e)),
    }
}

// ============================================================================
// FileEdit Tool
// ============================================================================

fn tool_file_edit(filename: &str, old_text: &str, new_text: &str) -> ToolResult {
    let resolved = match resolve_path_or_err(filename) {
        Ok(p) => p,
        Err(e) => return e,
    };
    
    // Read the file
    let fd = open(&resolved, open_flags::O_RDONLY);
    if fd < 0 {
        return ToolResult::err(&format!("Failed to open file: {}", filename));
    }

    let stat = match fstat(fd) {
        Ok(s) => s,
        Err(_) => {
            close(fd);
            return ToolResult::err("Failed to get file info");
        }
    };

    let size = stat.st_size as usize;
    if size > MAX_FILE_SIZE * 4 {
        close(fd);
        return ToolResult::err("File too large");
    }

    let mut buf = alloc::vec![0u8; size];
    let bytes_read = read_fd(fd, &mut buf);
    close(fd);

    if bytes_read <= 0 {
        return ToolResult::err("Failed to read file");
    }

    let content = match core::str::from_utf8(&buf[..bytes_read as usize]) {
        Ok(s) => String::from(s),
        Err(_) => return ToolResult::err("File contains non-UTF8 data"),
    };

    // Count occurrences
    let occurrences: Vec<_> = content.match_indices(old_text).collect();

    if occurrences.is_empty() {
        return ToolResult::err(&format!(
            "Text not found in '{}'. Make sure the text matches exactly (including whitespace).",
            filename
        ));
    }

    if occurrences.len() > 1 {
        let mut line_nums = Vec::new();
        for (pos, _) in &occurrences {
            let line_num = content[..*pos].matches('\n').count() + 1;
            line_nums.push(line_num);
        }
        return ToolResult::err(&format!(
            "Found {} occurrences at lines {:?}. Please provide more context to make the match unique.",
            occurrences.len(),
            line_nums
        ));
    }

    // Single match - perform replacement
    let (match_pos, _) = occurrences[0];
    let new_content = content.replace(old_text, new_text);

    // Write back
    let fd = open(
        &resolved,
        open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC,
    );
    if fd < 0 {
        return ToolResult::err(&format!("Failed to open file for writing: {}", filename));
    }

    let bytes_written = write_fd(fd, new_content.as_bytes());
    close(fd);

    if bytes_written < 0 {
        return ToolResult::err("Failed to write file");
    }

    // Find the line number of the change
    let line_num = content[..match_pos].matches('\n').count() + 1;

    // Create diff-like output
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();

    let mut diff = format!("Modified '{}' at line {}:\n```diff\n", filename, line_num);
    for line in &old_lines {
        diff.push_str(&format!("- {}\n", line));
    }
    for line in &new_lines {
        diff.push_str(&format!("+ {}\n", line));
    }
    diff.push_str("```");

    ToolResult::ok(diff)
}

// ============================================================================
// Shell Tool
// ============================================================================

fn tool_shell(command: &str) -> ToolResult {
    // Parse the command to get the binary and arguments
    // Simple tokenizer: split on whitespace, respecting quotes
    let tokens = tokenize_command(command);
    if tokens.is_empty() {
        return ToolResult::err("Empty command");
    }

    let binary = &tokens[0];
    let args: Vec<&str> = tokens.iter().map(|s| s.as_str()).collect();

    // Check for the binary in common paths
    let binary_path = if binary.starts_with('/') || binary.starts_with('.') {
        binary.clone()
    } else {
        // Try to find the binary
        let paths = ["/bin/", "/usr/bin/"];
        let mut found = None;
        for path in paths {
            let full_path = format!("{}{}", path, binary);
            let fd = open(&full_path, open_flags::O_RDONLY);
            if fd >= 0 {
                close(fd);
                found = Some(full_path);
                break;
            }
        }
        match found {
            Some(p) => p,
            None => {
                // Try the binary name directly
                binary.clone()
            }
        }
    };

    // Spawn the process
    let result = match spawn(&binary_path, Some(&args[..])) {
        Some(r) => r,
        None => return ToolResult::err(&format!("Failed to spawn '{}' (not found?)", binary_path)),
    };

    // Read output from child process
    let mut output = Vec::new();
    let mut buf = [0u8; 1024];
    let max_wait_ms = 30000;
    let mut waited_ms = 0u32;

    loop {
        let n = read_fd(result.stdout_fd as i32, &mut buf);
        if n > 0 {
            output.extend_from_slice(&buf[..n as usize]);
        }

        if let Some((_pid, exit_code)) = waitpid(result.pid) {
            // Drain remaining output
            loop {
                let n = read_fd(result.stdout_fd as i32, &mut buf);
                if n <= 0 {
                    break;
                }
                output.extend_from_slice(&buf[..n as usize]);
            }
            close(result.stdout_fd as i32);

            let output_str = core::str::from_utf8(&output).unwrap_or("<binary output>");

            let mut result_str = String::new();
            if !output_str.is_empty() {
                result_str.push_str("stdout:\n```\n");
                result_str.push_str(output_str);
                result_str.push_str("```\n");
            }
            result_str.push_str(&format!("\nExit code: {}", exit_code));

            if exit_code == 0 {
                return ToolResult::ok(result_str);
            } else {
                return ToolResult {
                    success: false,
                    output: result_str,
                };
            }
        }

        libakuma::sleep_ms(50);
        waited_ms += 50;

        if waited_ms >= max_wait_ms {
            close(result.stdout_fd as i32);
            return ToolResult::err("Command timed out after 30 seconds");
        }
    }
}

/// Tokenize a command string into arguments
fn tokenize_command(cmd: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escape_next = false;

    for c in cmd.chars() {
        if escape_next {
            current.push(c);
            escape_next = false;
            continue;
        }

        match c {
            '\\' if !in_single_quote => {
                escape_next = true;
            }
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
            }
            ' ' | '\t' if !in_single_quote && !in_double_quote => {
                if !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
            }
            _ => {
                current.push(c);
            }
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}
