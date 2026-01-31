//! Code search module for meow-local
//!
//! Provides grep-like search functionality for Rust source files.

use regex::Regex;
use std::fs;
use std::io::{self, BufRead};
use std::path::Path;

/// Maximum number of matches to return (to avoid overwhelming output)
const MAX_MATCHES: usize = 50;

/// Search for a pattern in Rust files recursively
///
/// # Arguments
/// * `pattern` - Regex pattern to search for
/// * `directory` - Root directory to search in
/// * `context_lines` - Number of lines of context to show before/after matches
///
/// # Returns
/// A formatted string with all matches, or an error
pub fn search_to_string(
    pattern: &str,
    directory: &str,
    context_lines: usize,
) -> io::Result<String> {
    let regex = Regex::new(pattern).map_err(|e| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("Invalid regex: {}", e))
    })?;

    let mut matches: Vec<(String, usize, Vec<String>)> = Vec::new(); // (file, line_num, context_lines)
    let path = Path::new(directory);

    search_recursive(path, &regex, context_lines, &mut matches)?;

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

    for (file, line_num, context) in display_matches {
        output.push_str(&format!("{}:{}\n", file, line_num));
        for line in context {
            output.push_str(line);
            output.push('\n');
        }
        output.push('\n');
    }

    Ok(output)
}

/// Recursively search through directories
fn search_recursive(
    path: &Path,
    regex: &Regex,
    context_lines: usize,
    matches: &mut Vec<(String, usize, Vec<String>)>,
) -> io::Result<()> {
    if matches.len() >= MAX_MATCHES * 2 {
        // Stop early if we have way more than we need
        return Ok(());
    }

    if path.is_file() {
        if let Some(ext) = path.extension() {
            if ext == "rs" {
                search_file(path, regex, context_lines, matches)?;
            }
        }
    } else if path.is_dir() {
        // Skip common non-source directories
        let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if dir_name == "target" || dir_name == ".git" || dir_name == "node_modules" {
            return Ok(());
        }

        for entry in fs::read_dir(path)? {
            let entry = entry?;
            search_recursive(&entry.path(), regex, context_lines, matches)?;
        }
    }

    Ok(())
}

/// Search a single file for matches
fn search_file(
    path: &Path,
    regex: &Regex,
    context_lines: usize,
    matches: &mut Vec<(String, usize, Vec<String>)>,
) -> io::Result<()> {
    let file = fs::File::open(path)?;
    let reader = io::BufReader::new(file);
    let lines: Vec<String> = reader.lines().collect::<io::Result<Vec<_>>>()?;

    let file_path = path.display().to_string();

    for (idx, line) in lines.iter().enumerate() {
        if regex.is_match(line) {
            let line_num = idx + 1; // 1-indexed

            // Collect context lines
            let start = idx.saturating_sub(context_lines);
            let end = (idx + context_lines + 1).min(lines.len());

            let mut context = Vec::new();
            for i in start..end {
                let prefix = if i == idx { ">" } else { " " };
                context.push(format!("{} {:>4}: {}", prefix, i + 1, lines[i]));
            }

            matches.push((file_path.clone(), line_num, context));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_pattern() {
        // This test requires actual files, so just verify regex compilation
        let result = search_to_string("fn main", ".", 0);
        assert!(result.is_ok());
    }

    #[test]
    fn test_invalid_regex() {
        let result = search_to_string("[invalid", ".", 0);
        assert!(result.is_err());
    }
}
