//! Path manipulation utilities.
//!
//! `resolve_path` runs on **every** VFS operation (`vfs::resolve_mount` calls it
//! before touching any mount), so both entry points here are deliberately
//! single-allocation: the returned `String` is the only thing allocated. They
//! used to cost three — a `format!` to join, a scratch `Vec<&str>` to hold
//! components, and the result — which is why the component walk below writes
//! into the output buffer directly and handles `..` by truncating it.

use alloc::string::String;

/// Append `path`'s components to `out`, resolving `.` and `..` as it goes.
///
/// `out` is always either empty or of the form `/a/b` — no trailing slash — so
/// `..` is just "cut at the last slash". Popping when `out` is empty is a no-op,
/// which is what makes `/..` and `../..` both resolve to `/` (the `Vec::pop`
/// this replaced behaved the same way).
fn push_components(out: &mut String, path: &str) {
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if let Some(idx) = out.rfind('/') {
                    out.truncate(idx);
                }
            }
            c => {
                out.push('/');
                out.push_str(c);
            }
        }
    }
}

/// Normalize a path: resolve `.` and `..` components.
#[must_use]
pub fn canonicalize_path(path: &str) -> String {
    // A canonical path is never longer than its input plus a leading slash.
    let mut out = String::with_capacity(path.len() + 1);
    push_components(&mut out, path);
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// Resolve a path relative to a base directory.
#[must_use]
pub fn resolve_path(base_cwd: &str, path: &str) -> String {
    if path.starts_with('/') {
        return canonicalize_path(path);
    }
    // Walking the two halves in sequence is exactly canonicalizing
    // `"{base_cwd}/{path}"` — component order is identical and empty
    // components are skipped either way — but with no temporary to join them.
    // `..` still crosses the boundary correctly: base `/a` + `../b` truncates
    // `/a` away before pushing `/b`.
    let mut out = String::with_capacity(base_cwd.len() + path.len() + 2);
    push_components(&mut out, base_cwd);
    push_components(&mut out, path);
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// Split a path into (`parent_path`, `filename`).
#[must_use]
pub fn split_path(path: &str) -> (&str, &str) {
    let path = path.trim_start_matches('/').trim_end_matches('/');
    path.rfind('/').map_or(("", path), |idx| (&path[..idx], &path[idx + 1..]))
}

/// Split a path into components, without allocating.
///
/// Returns an iterator rather than a `Vec`: every caller either loops or
/// `collect`s on its own terms, and the vector was pure overhead.
pub fn path_components(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|s| !s.is_empty())
}
