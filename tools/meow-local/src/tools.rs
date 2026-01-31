//! Tool execution module for Meow-chan (local/native mode)
//!
//! Implements file system, network, and shell tools for local execution.
//! Includes security measures like path sandboxing for the shell tool.

use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use crate::code_search;

/// Result of a tool execution
pub struct ToolResult {
    pub success: bool,
    pub output: String,
}

impl ToolResult {
    pub fn ok(output: String) -> Self {
        Self {
            success: true,
            output,
        }
    }

    pub fn err(message: &str) -> Self {
        Self {
            success: false,
            output: String::from(message),
        }
    }
}

/// The sandbox root directory for shell commands
static SANDBOX_ROOT: OnceLock<PathBuf> = OnceLock::new();

#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
static TEST_SANDBOX_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Initialize the sandbox root directory
pub fn init_sandbox(root: PathBuf) {
    #[cfg(test)]
    {
        *TEST_SANDBOX_ROOT.lock().unwrap() = Some(root);
        return;
    }
    #[cfg(not(test))]
    {
        let _ = SANDBOX_ROOT.set(root);
    }
}

/// Get the sandbox root directory
fn get_sandbox_root() -> PathBuf {
    #[cfg(test)]
    {
        if let Ok(guard) = TEST_SANDBOX_ROOT.lock() {
            if let Some(ref path) = *guard {
                return path.clone();
            }
        }
    }
    SANDBOX_ROOT
        .get()
        .cloned()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Check if a path is within the sandbox root
/// Returns the canonicalized path if valid, or an error message
fn validate_path_in_sandbox(path: &str) -> Result<PathBuf, String> {
    let sandbox_root = get_sandbox_root();
    let canonical_root = sandbox_root
        .canonicalize()
        .map_err(|e| format!("Failed to resolve sandbox root: {}", e))?;

    // Resolve the path relative to sandbox root
    let target_path = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        sandbox_root.join(path)
    };

    // Canonicalize to resolve .. and symlinks
    // For new files, we check the parent directory
    let check_path = if target_path.exists() {
        target_path
            .canonicalize()
            .map_err(|e| format!("Failed to resolve path: {}", e))?
    } else {
        // For non-existent paths, check the parent
        if let Some(parent) = target_path.parent() {
            if parent.exists() {
                let canonical_parent = parent
                    .canonicalize()
                    .map_err(|e| format!("Failed to resolve parent: {}", e))?;
                canonical_parent.join(target_path.file_name().unwrap_or_default())
            } else {
                return Err(format!("Parent directory does not exist: {:?}", parent));
            }
        } else {
            return Err("Invalid path".to_string());
        }
    };

    // Verify the path is within sandbox
    if check_path.starts_with(&canonical_root) {
        Ok(check_path)
    } else {
        Err(format!(
            "Access denied: path '{}' is outside sandbox root '{}'",
            path,
            canonical_root.display()
        ))
    }
}

/// Parse and execute a tool command from JSON
pub fn execute_tool_command(json: &str) -> Option<ToolResult> {
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
            let path = extract_string_field(json, "path").unwrap_or_else(|| String::from("."));
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
        "Shell" => {
            let cmd = extract_string_field(json, "cmd")?;
            Some(tool_shell(&cmd))
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
        _ => None,
    }
}

/// Try to find and execute a tool command in the LLM's response
pub fn find_and_execute_tool(response: &str) -> (String, Option<ToolResult>) {
    // Look for JSON code block with command
    if let Some(start) = response.find("```json") {
        if let Some(end) = response[start..]
            .find("```\n")
            .or_else(|| response[start..].rfind("```"))
        {
            let json_start = start + 7;
            let json_end = start + end;

            if json_start < json_end && json_end <= response.len() {
                let json_block = response[json_start..json_end].trim();

                if json_block.contains("\"command\"") && json_block.contains("\"tool\"") {
                    if let Some(result) = execute_tool_command(json_block) {
                        let before = response[..start].trim();
                        return (String::from(before), Some(result));
                    }
                }
            }
        }
    }

    // Also try inline JSON
    if let Some(start) = response.find("{\"command\"") {
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

const MAX_FILE_SIZE: usize = 1000 * 1024; // 1MB

fn tool_file_read(filename: &str) -> ToolResult {
    match validate_path_in_sandbox(filename) {
        Ok(path) => match std::fs::read_to_string(&path) {
            Ok(content) => {
                if content.len() > MAX_FILE_SIZE {
                    ToolResult::err("File too large (max 1MB)")
                } else {
                    ToolResult::ok(format!(
                        "Contents of '{}':\n```\n{}\n```",
                        filename, content
                    ))
                }
            }
            Err(e) => ToolResult::err(&format!("Failed to read file: {}", e)),
        },
        Err(e) => ToolResult::err(&e),
    }
}

fn tool_file_write(filename: &str, content: &str) -> ToolResult {
    match validate_path_in_sandbox(filename) {
        Ok(path) => match std::fs::write(&path, content) {
            Ok(_) => ToolResult::ok(format!(
                "Successfully wrote {} bytes to '{}'",
                content.len(),
                filename
            )),
            Err(e) => ToolResult::err(&format!("Failed to write file: {}", e)),
        },
        Err(e) => ToolResult::err(&e),
    }
}

fn tool_file_append(filename: &str, content: &str) -> ToolResult {
    match validate_path_in_sandbox(filename) {
        Ok(path) => {
            use std::io::Write;
            match std::fs::OpenOptions::new().append(true).open(&path) {
                Ok(mut file) => match file.write_all(content.as_bytes()) {
                    Ok(_) => ToolResult::ok(format!(
                        "Successfully appended {} bytes to '{}'",
                        content.len(),
                        filename
                    )),
                    Err(e) => ToolResult::err(&format!("Failed to append: {}", e)),
                },
                Err(e) => ToolResult::err(&format!("Failed to open file: {}", e)),
            }
        }
        Err(e) => ToolResult::err(&e),
    }
}

fn tool_file_exists(filename: &str) -> ToolResult {
    match validate_path_in_sandbox(filename) {
        Ok(path) => {
            if path.exists() {
                ToolResult::ok(format!("'{}' exists", filename))
            } else {
                ToolResult::ok(format!("'{}' does not exist", filename))
            }
        }
        Err(e) => ToolResult::err(&e),
    }
}

fn tool_file_list(path: &str) -> ToolResult {
    match validate_path_in_sandbox(path) {
        Ok(dir_path) => match std::fs::read_dir(&dir_path) {
            Ok(entries) => {
                let mut output = format!("Contents of '{}':\n", path);
                let mut count = 0;
                for entry in entries.flatten() {
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    let type_indicator = if is_dir { "/" } else { "" };
                    output.push_str(&format!(
                        "  {}{}\n",
                        entry.file_name().to_string_lossy(),
                        type_indicator
                    ));
                    count += 1;
                }
                if count == 0 {
                    output.push_str("  (empty directory)\n");
                }
                ToolResult::ok(output)
            }
            Err(e) => ToolResult::err(&format!("Failed to list directory: {}", e)),
        },
        Err(e) => ToolResult::err(&e),
    }
}

fn tool_file_delete(filename: &str) -> ToolResult {
    match validate_path_in_sandbox(filename) {
        Ok(path) => match std::fs::remove_file(&path) {
            Ok(_) => ToolResult::ok(format!("Successfully deleted '{}'", filename)),
            Err(e) => ToolResult::err(&format!("Failed to delete: {}", e)),
        },
        Err(e) => ToolResult::err(&e),
    }
}

fn tool_folder_create(path: &str) -> ToolResult {
    match validate_path_in_sandbox(path) {
        Ok(dir_path) => match std::fs::create_dir_all(&dir_path) {
            Ok(_) => ToolResult::ok(format!("Successfully created directory: '{}'", path)),
            Err(e) => ToolResult::err(&format!("Failed to create directory: {}", e)),
        },
        Err(e) => ToolResult::err(&e),
    }
}

fn tool_file_rename(source: &str, dest: &str) -> ToolResult {
    let src_path = match validate_path_in_sandbox(source) {
        Ok(p) => p,
        Err(e) => return ToolResult::err(&e),
    };
    let dst_path = match validate_path_in_sandbox(dest) {
        Ok(p) => p,
        Err(e) => return ToolResult::err(&e),
    };

    match std::fs::rename(&src_path, &dst_path) {
        Ok(_) => ToolResult::ok(format!("Renamed '{}' to '{}'", source, dest)),
        Err(e) => ToolResult::err(&format!("Failed to rename: {}", e)),
    }
}

fn tool_file_copy(source: &str, dest: &str) -> ToolResult {
    let src_path = match validate_path_in_sandbox(source) {
        Ok(p) => p,
        Err(e) => return ToolResult::err(&e),
    };
    let dst_path = match validate_path_in_sandbox(dest) {
        Ok(p) => p,
        Err(e) => return ToolResult::err(&e),
    };

    match std::fs::copy(&src_path, &dst_path) {
        Ok(bytes) => ToolResult::ok(format!(
            "Copied '{}' to '{}' ({} bytes)",
            source, dest, bytes
        )),
        Err(e) => ToolResult::err(&format!("Failed to copy: {}", e)),
    }
}

fn tool_file_move(source: &str, dest: &str) -> ToolResult {
    let src_path = match validate_path_in_sandbox(source) {
        Ok(p) => p,
        Err(e) => return ToolResult::err(&e),
    };
    let dst_path = match validate_path_in_sandbox(dest) {
        Ok(p) => p,
        Err(e) => return ToolResult::err(&e),
    };

    // Try rename first (atomic if on same filesystem)
    if std::fs::rename(&src_path, &dst_path).is_ok() {
        return ToolResult::ok(format!("Moved '{}' to '{}'", source, dest));
    }

    // Fall back to copy + delete
    match std::fs::copy(&src_path, &dst_path) {
        Ok(_) => {
            let _ = std::fs::remove_file(&src_path);
            ToolResult::ok(format!("Moved '{}' to '{}'", source, dest))
        }
        Err(e) => ToolResult::err(&format!("Failed to move: {}", e)),
    }
}

// ============================================================================
// HTTP Fetch Tool
// ============================================================================

const MAX_FETCH_SIZE: usize = 64 * 1024;

fn tool_http_fetch(url: &str) -> ToolResult {
    // Parse URL
    let (is_https, host, port, path) = match parse_url(url) {
        Some(parsed) => parsed,
        None => return ToolResult::err("Invalid URL format. Use: http(s)://host[:port]/path"),
    };

    let addr = format!("{}:{}", host, port);

    // Connect
    let mut stream = match std::net::TcpStream::connect(&addr) {
        Ok(s) => s,
        Err(e) => return ToolResult::err(&format!("Connection failed: {}", e)),
    };

    // For HTTPS, we'd need TLS - for now just support HTTP
    if is_https {
        return ToolResult::err("HTTPS not supported in local mode (use HTTP)");
    }

    // Build HTTP request
    let request = format!(
        "GET {} HTTP/1.0\r\n\
         Host: {}\r\n\
         User-Agent: meow-local/1.0\r\n\
         Connection: close\r\n\
         \r\n",
        path, host
    );

    // Send request
    use std::io::Write;
    if let Err(e) = stream.write_all(request.as_bytes()) {
        return ToolResult::err(&format!("Failed to send request: {}", e));
    }

    // Read response
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if response.len() + n > MAX_FETCH_SIZE {
                    let remaining = MAX_FETCH_SIZE - response.len();
                    response.extend_from_slice(&buf[..remaining]);
                    break;
                }
                response.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    if response.is_empty() {
        return ToolResult::err("Empty response from server");
    }

    // Parse response
    let response_str = String::from_utf8_lossy(&response);
    if let Some(body_start) = response_str.find("\r\n\r\n") {
        let body = &response_str[body_start + 4..];
        let truncated = if response.len() >= MAX_FETCH_SIZE {
            " (truncated)"
        } else {
            ""
        };
        ToolResult::ok(format!(
            "Fetched {} ({} bytes{}):\n```\n{}\n```",
            url,
            body.len(),
            truncated,
            body
        ))
    } else {
        ToolResult::err("Failed to parse HTTP response")
    }
}

fn parse_url(url: &str) -> Option<(bool, &str, u16, &str)> {
    let (is_https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return None;
    };

    let default_port = if is_https { 443 } else { 80 };

    let (host_port, path) = match rest.find('/') {
        Some(pos) => (&rest[..pos], &rest[pos..]),
        None => (rest, "/"),
    };

    let (host, port) = match host_port.rfind(':') {
        Some(pos) => {
            let h = &host_port[..pos];
            let p = host_port[pos + 1..].parse::<u16>().ok()?;
            (h, p)
        }
        None => (host_port, default_port),
    };

    Some((is_https, host, port, path))
}

// ============================================================================
// Shell Tool (with sandboxing)
// ============================================================================

/// Execute a shell command within the sandbox directory
///
/// Security measures:
/// - Commands run in the sandbox root directory
/// - Cannot use cd to escape the sandbox
/// - Output is captured and returned
fn tool_shell(command: &str) -> ToolResult {
    let sandbox_root = get_sandbox_root();

    // Validate the sandbox root exists
    if !sandbox_root.exists() {
        return ToolResult::err(&format!("Sandbox root does not exist: {:?}", sandbox_root));
    }

    // Check for obviously dangerous patterns
    // Note: This is defense-in-depth, the real protection is running in the sandbox dir
    let dangerous_patterns = [
        "rm -rf /",
        "rm -rf ~",
        ":(){ :|:& };:", // Fork bomb
        "> /dev/",
        "dd if=",
        "mkfs",
        "chmod -R 777 /",
    ];

    for pattern in dangerous_patterns {
        if command.contains(pattern) {
            return ToolResult::err(&format!(
                "Potentially dangerous command blocked: contains '{}'",
                pattern
            ));
        }
    }

    // Create a wrapper script that:
    // 1. Changes to the sandbox directory
    // 2. Intercepts cd commands to prevent escaping
    // 3. Runs the command

    let wrapped_command = format!(
        r#"
        # Function to validate cd targets
        safe_cd() {{
            local target="$1"
            local resolved

            # Resolve the path
            if [[ "$target" = /* ]]; then
                resolved="$target"
            else
                resolved="$(pwd)/$target"
            fi

            # Canonicalize
            resolved="$(cd "$resolved" 2>/dev/null && pwd)" || {{
                echo "cd: no such directory: $target" >&2
                return 1
            }}

            # Check if within sandbox
            if [[ "$resolved" != "{sandbox}"* ]]; then
                echo "cd: access denied: cannot leave sandbox" >&2
                return 1
            fi

            builtin cd "$resolved"
        }}

        # Override cd
        cd() {{ safe_cd "$@"; }}

        # Run in sandbox
        builtin cd "{sandbox}" || exit 1

        # Execute the command
        {command}
        "#,
        sandbox = sandbox_root.display(),
        command = command
    );

    // Execute via bash
    let output = Command::new("/bin/bash")
        .arg("-c")
        .arg(&wrapped_command)
        .current_dir(&sandbox_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            let mut result = String::new();

            if !stdout.is_empty() {
                result.push_str("stdout:\n```\n");
                result.push_str(&stdout);
                result.push_str("```\n");
            }

            if !stderr.is_empty() {
                result.push_str("stderr:\n```\n");
                result.push_str(&stderr);
                result.push_str("```\n");
            }

            if result.is_empty() {
                result = "(no output)".to_string();
            }

            result.push_str(&format!(
                "\nExit code: {}",
                output.status.code().unwrap_or(-1)
            ));

            if output.status.success() {
                ToolResult::ok(result)
            } else {
                ToolResult {
                    success: false,
                    output: result,
                }
            }
        }
        Err(e) => ToolResult::err(&format!("Failed to execute command: {}", e)),
    }
}

// ============================================================================
// FileReadLines Tool
// ============================================================================

fn tool_file_read_lines(filename: &str, start: usize, end: usize) -> ToolResult {
    match validate_path_in_sandbox(filename) {
        Ok(path) => {
            let file = match std::fs::File::open(&path) {
                Ok(f) => f,
                Err(e) => return ToolResult::err(&format!("Failed to open file: {}", e)),
            };

            let reader = std::io::BufReader::new(file);
            let lines: Vec<String> = match reader.lines().collect() {
                Ok(l) => l,
                Err(e) => return ToolResult::err(&format!("Failed to read file: {}", e)),
            };

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
        Err(e) => ToolResult::err(&e),
    }
}

// ============================================================================
// CodeSearch Tool
// ============================================================================

fn tool_code_search(pattern: &str, path: &str, context: usize) -> ToolResult {
    match validate_path_in_sandbox(path) {
        Ok(search_path) => {
            match code_search::search_to_string(
                pattern,
                search_path.to_str().unwrap_or("."),
                context,
            ) {
                Ok(results) => ToolResult::ok(results),
                Err(e) => ToolResult::err(&format!("Search failed: {}", e)),
            }
        }
        Err(e) => ToolResult::err(&e),
    }
}

// ============================================================================
// FileEdit Tool
// ============================================================================

fn tool_file_edit(filename: &str, old_text: &str, new_text: &str) -> ToolResult {
    match validate_path_in_sandbox(filename) {
        Ok(path) => {
            // Read the file
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => return ToolResult::err(&format!("Failed to read file: {}", e)),
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
                // Find line numbers for each occurrence
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
            if let Err(e) = std::fs::write(&path, &new_content) {
                return ToolResult::err(&format!("Failed to write file: {}", e));
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
        Err(e) => ToolResult::err(&e),
    }
}

// ============================================================================
// JSON Parsing Helpers
// ============================================================================

/// Extract a number field from JSON
fn extract_number_field(json: &str, field: &str) -> Option<usize> {
    let pattern = format!("\"{}\"", field);
    let start = json.find(&pattern)?;

    let after_field = &json[start + pattern.len()..];
    let colon_pos = after_field.find(':')?;
    let after_colon = &after_field[colon_pos + 1..];

    let trimmed = after_colon.trim_start();

    // Find the number
    let num_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    if num_end == 0 {
        return None;
    }

    trimmed[..num_end].parse().ok()
}

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

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn setup_test_sandbox() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("meow_test_{}_{}", std::process::id(), id));
        fs::create_dir_all(&dir).unwrap();
        init_sandbox(dir.clone());
        dir
    }

    fn cleanup_test_sandbox(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    // ========== FileReadLines Tests ==========

    #[test]
    fn test_file_read_lines_basic() {
        let dir = setup_test_sandbox();
        let test_file = dir.join("test.rs");

        let content = "line 1\nline 2\nline 3\nline 4\nline 5\n";
        fs::write(&test_file, content).unwrap();

        let result = tool_file_read_lines(test_file.to_str().unwrap(), 2, 4);
        assert!(result.success, "Failed: {}", result.output);
        assert!(result.output.contains("line 2"));
        assert!(result.output.contains("line 3"));
        assert!(result.output.contains("line 4"));
        assert!(result.output.contains("2:"));
        assert!(result.output.contains("3:"));

        cleanup_test_sandbox(&dir);
    }

    #[test]
    fn test_file_read_lines_beyond_file() {
        let dir = setup_test_sandbox();
        let test_file = dir.join("short.rs");

        fs::write(&test_file, "line 1\nline 2\n").unwrap();

        let result = tool_file_read_lines(test_file.to_str().unwrap(), 100, 150);
        assert!(!result.success);
        assert!(result.output.contains("beyond file length"));

        cleanup_test_sandbox(&dir);
    }

    #[test]
    fn test_file_read_lines_nonexistent() {
        let dir = setup_test_sandbox();
        let nonexistent = dir.join("nonexistent.rs");

        let result = tool_file_read_lines(nonexistent.to_str().unwrap(), 1, 10);
        assert!(!result.success);

        cleanup_test_sandbox(&dir);
    }

    // ========== FileEdit Tests ==========

    #[test]
    fn test_file_edit_single_match() {
        let dir = setup_test_sandbox();
        let test_file = dir.join("edit_test.rs");

        fs::write(&test_file, "fn old_function() {\n    // body\n}\n").unwrap();

        let result = tool_file_edit(test_file.to_str().unwrap(), "old_function", "new_function");
        assert!(result.success, "Failed: {}", result.output);
        assert!(result.output.contains("- old_function"));
        assert!(result.output.contains("+ new_function"));

        let content = fs::read_to_string(&test_file).unwrap();
        assert!(content.contains("new_function"));
        assert!(!content.contains("old_function"));

        cleanup_test_sandbox(&dir);
    }

    #[test]
    fn test_file_edit_no_match() {
        let dir = setup_test_sandbox();
        let test_file = dir.join("no_match.rs");

        fs::write(&test_file, "fn some_function() {}\n").unwrap();

        let result = tool_file_edit(
            test_file.to_str().unwrap(),
            "nonexistent_text",
            "replacement",
        );
        assert!(!result.success);
        assert!(result.output.contains("not found"));

        cleanup_test_sandbox(&dir);
    }

    #[test]
    fn test_file_edit_multiple_matches() {
        let dir = setup_test_sandbox();
        let test_file = dir.join("multi_match.rs");

        fs::write(&test_file, "let x = 1;\nlet y = 1;\nlet z = 1;\n").unwrap();

        let result = tool_file_edit(test_file.to_str().unwrap(), "= 1", "= 2");
        assert!(!result.success);
        assert!(result.output.contains("3 occurrences"));

        cleanup_test_sandbox(&dir);
    }

    #[test]
    fn test_file_edit_multiline() {
        let dir = setup_test_sandbox();
        let test_file = dir.join("multiline.rs");

        let original = "fn test() {\n    let x = 1;\n    let y = 2;\n}\n";
        fs::write(&test_file, original).unwrap();

        let result = tool_file_edit(
            test_file.to_str().unwrap(),
            "let x = 1;\n    let y = 2;",
            "let x = 10;\n    let y = 20;",
        );
        assert!(result.success, "Failed: {}", result.output);

        let content = fs::read_to_string(&test_file).unwrap();
        assert!(content.contains("let x = 10;"));
        assert!(content.contains("let y = 20;"));

        cleanup_test_sandbox(&dir);
    }

    // ========== CodeSearch Tests ==========

    #[test]
    fn test_code_search_finds_pattern() {
        let dir = setup_test_sandbox();
        let test_file = dir.join("searchable.rs");

        fs::write(
            &test_file,
            "fn my_unique_function() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();

        let result = tool_code_search("my_unique_function", dir.to_str().unwrap(), 1);
        assert!(result.success, "Failed: {}", result.output);
        assert!(result.output.contains("my_unique_function"));
        assert!(result.output.contains("searchable.rs"));

        cleanup_test_sandbox(&dir);
    }

    #[test]
    fn test_code_search_no_matches() {
        let dir = setup_test_sandbox();
        let test_file = dir.join("empty_search.rs");

        fs::write(&test_file, "fn something_else() {}\n").unwrap();

        let result = tool_code_search("nonexistent_pattern_xyz", dir.to_str().unwrap(), 0);
        assert!(result.success);
        assert!(result.output.contains("No matches found"));

        cleanup_test_sandbox(&dir);
    }

    #[test]
    fn test_code_search_regex() {
        let dir = setup_test_sandbox();
        let test_file = dir.join("regex_test.rs");

        fs::write(
            &test_file,
            "fn foo_bar() {}\nfn foo_baz() {}\nfn other() {}\n",
        )
        .unwrap();

        let result = tool_code_search(r"fn foo_\w+", dir.to_str().unwrap(), 0);
        assert!(result.success, "Failed: {}", result.output);
        assert!(result.output.contains("foo_bar"));
        assert!(result.output.contains("foo_baz"));

        cleanup_test_sandbox(&dir);
    }

    #[test]
    fn test_code_search_invalid_regex() {
        let dir = setup_test_sandbox();

        let result = tool_code_search("[invalid", dir.to_str().unwrap(), 0);
        assert!(!result.success);
        assert!(result.output.contains("Invalid regex"));

        cleanup_test_sandbox(&dir);
    }

    // ========== JSON Parsing Tests ==========

    #[test]
    fn test_extract_number_field() {
        assert_eq!(extract_number_field(r#"{"start": 42}"#, "start"), Some(42));
        assert_eq!(extract_number_field(r#"{"start": 0}"#, "start"), Some(0));
        assert_eq!(
            extract_number_field(r#"{"start": 100, "end": 200}"#, "end"),
            Some(200)
        );
        assert_eq!(extract_number_field(r#"{"other": 5}"#, "start"), None);
    }

    // ========== Sandbox Tests ==========

    #[test]
    fn test_sandbox_prevents_escape() {
        let dir = setup_test_sandbox();

        // Try to read outside sandbox with absolute path
        let result = tool_file_read_lines("/etc/passwd", 1, 10);
        assert!(!result.success);
        assert!(
            result.output.contains("outside sandbox") || result.output.contains("Access denied"),
            "Expected sandbox error, got: {}",
            result.output
        );

        cleanup_test_sandbox(&dir);
    }
}


/// Compare two files using the `diff` command.
fn tool_diff_files(source: &str, destination: &str) -> ToolResult {
    match (validate_path_in_sandbox(source), validate_path_in_sandbox(destination)) {
        (Ok(src_path), Ok(dest_path)) => {
            let output = Command::new("diff")
                .arg(&src_path)
                .arg(&dest_path)
                .output();

            match output {
                Ok(output) => {
                    if output.status.success() {
                        ToolResult::ok(String::from_utf8_lossy(&output.stdout).to_string())
                    } else {
                        ToolResult::err(&format!("Diff command failed: {}", String::from_utf8_lossy(&output.stderr)))
                    }
                }
                Err(e) => ToolResult::err(&format!("Failed to execute diff command: {}", e)),
            }
        }
        (Err(e), _ ) => ToolResult::err(&e),
        (_, Err(e)) => ToolResult::err(&e),
    }
}

