//! Path manipulation utilities.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Normalize a path: resolve `.` and `..` components.
#[must_use]
pub fn canonicalize_path(path: &str) -> String {
    let mut components: Vec<&str> = Vec::new();

    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            c => {
                components.push(c);
            }
        }
    }

    if components.is_empty() {
        String::from("/")
    } else {
        let mut result = String::new();
        for c in components {
            result.push('/');
            result.push_str(c);
        }
        result
    }
}

/// Resolve a path relative to a base directory.
#[must_use]
pub fn resolve_path(base_cwd: &str, path: &str) -> String {
    if path.starts_with('/') {
        canonicalize_path(path)
    } else {
        let full_path = if base_cwd == "/" {
            format!("/{path}")
        } else {
            format!("{base_cwd}/{path}")
        };
        canonicalize_path(&full_path)
    }
}

/// Split a path into (`parent_path`, `filename`).
#[must_use]
pub fn split_path(path: &str) -> (&str, &str) {
    let path = path.trim_start_matches('/').trim_end_matches('/');
    path.rfind('/').map_or(("", path), |idx| (&path[..idx], &path[idx + 1..]))
}

/// Split path into components.
#[must_use]
pub fn path_components(path: &str) -> Vec<&str> {
    path.trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect()
}
