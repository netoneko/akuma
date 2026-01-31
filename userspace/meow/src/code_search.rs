//! Code search module for meow
//!
//! Provides grep-like search functionality for Rust source files.
//! Uses simple string matching (no regex) to stay no_std compatible.

use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;

use libakuma::{open, close, read_fd, fstat, read_dir, open_flags};

/// Maximum number of matches to return (to avoid overwhelming output)
const MAX_MATCHES: usize = 50;
/// Maximum file size to search (256KB)
const MAX_FILE_SIZE: usize = 256 * 1024;

/// Search for a pattern in Rust files recursively
///
/// # Arguments
/// * `pattern` - Text pattern to search for (simple string matching)
/// * `directory` - Root directory to search in
/// * `context_lines` - Number of lines of context to show before/after matches
///
/// # Returns
/// A formatted string with all matches, or an error
pub fn search_to_string(
    pattern: &str,
    directory: &str,
    context_lines: usize,
) -> Result<String, &'static str> {
    if pattern.is_empty() {
        return Err("Empty search pattern");
    }

    let mut matches: Vec<Match> = Vec::new();
    search_recursive(directory, pattern, context_lines, &mut matches);

    if matches.is_empty() {
        return Ok(format!("No matches found for pattern: {}", pattern));
    }

    let total_matches = matches.len();
    let truncated = total_matches > MAX_MATCHES;
    let display_matches = if truncated {
        &matches[..MAX_MATCHES]
    } else {
        &matches[..]
    };

    let mut output = String::new();
    output.push_str(&format!(
        "Found {} matches for '{}'",
        total_matches, pattern
    ));
    if truncated {
        output.push_str(&format!(" (showing first {})", MAX_MATCHES));
    }
    output.push_str(":\n\n");

    for m in display_matches {
        output.push_str(&format!("{}:{}\n", m.file, m.line_num));
        for line in &m.context {
            output.push_str(line);
            output.push('\n');
        }
        output.push('\n');
    }

    Ok(output)
}

/// A single match result
struct Match {
    file: String,
    line_num: usize,
    context: Vec<String>,
}

/// Recursively search through directories
fn search_recursive(
    path: &str,
    pattern: &str,
    context_lines: usize,
    matches: &mut Vec<Match>,
) {
    if matches.len() >= MAX_MATCHES * 2 {
        // Stop early if we have way more than we need
        return;
    }

    // Check if path is a directory
    if let Some(entries) = read_dir(path) {
        for entry in entries {
            // Skip common non-source directories
            if entry.name == "target" || entry.name == ".git" || entry.name == "node_modules" {
                continue;
            }

            let full_path = if path.ends_with('/') {
                format!("{}{}", path, entry.name)
            } else {
                format!("{}/{}", path, entry.name)
            };

            if entry.is_dir {
                search_recursive(&full_path, pattern, context_lines, matches);
            } else if entry.name.ends_with(".rs") {
                search_file(&full_path, pattern, context_lines, matches);
            }
        }
    } else {
        // Not a directory, try as a file
        if path.ends_with(".rs") {
            search_file(path, pattern, context_lines, matches);
        }
    }
}

/// Search a single file for matches
fn search_file(
    path: &str,
    pattern: &str,
    context_lines: usize,
    matches: &mut Vec<Match>,
) {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        return;
    }

    // Get file size
    let stat = match fstat(fd) {
        Ok(s) => s,
        Err(_) => {
            close(fd);
            return;
        }
    };

    let size = stat.st_size as usize;
    if size == 0 || size > MAX_FILE_SIZE {
        close(fd);
        return;
    }

    let mut buf = alloc::vec![0u8; size];
    let bytes_read = read_fd(fd, &mut buf);
    close(fd);

    if bytes_read <= 0 {
        return;
    }

    let content = match core::str::from_utf8(&buf[..bytes_read as usize]) {
        Ok(s) => s,
        Err(_) => return,
    };

    // Split into lines
    let lines: Vec<&str> = content.lines().collect();

    for (idx, line) in lines.iter().enumerate() {
        if line.contains(pattern) {
            let line_num = idx + 1; // 1-indexed

            // Collect context lines
            let start = idx.saturating_sub(context_lines);
            let end = (idx + context_lines + 1).min(lines.len());

            let mut context = Vec::new();
            for i in start..end {
                let prefix = if i == idx { ">" } else { " " };
                context.push(format!("{} {:>4}: {}", prefix, i + 1, lines[i]));
            }

            matches.push(Match {
                file: String::from(path),
                line_num,
                context,
            });
        }
    }
}
