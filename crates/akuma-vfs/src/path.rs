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

/// Walk `path` component by component, following symlinks **anywhere** in it.
///
/// `read_link(prefix)` answers "is `prefix` a symlink, and to what" — the
/// caller's filesystem owns that; this function owns the *splicing*: a
/// symlink's target takes the link's place in the walk and every component
/// that came **after** the link is walked against the target, which is the
/// part the final-component-only resolver lost (`/bin/sh` resolved to
/// `/usr/bin` and dropped the `sh`).
///
/// A relative target resolves against the link's own directory; an absolute
/// one restarts the walk at `/`. After `max_hops` expansions the walk stops
/// at the deepest prefix reached — the same give-up shape the previous
/// resolver applied to every path (Linux caps at 40; the callers here pass
/// 16, which is more than any real chain on either kernel: a shebang chain
/// is at most two).
///
/// `.` and `..` are normalized **before** the walk, exactly as the previous
/// resolver did. That is textual, so `/a/link/../z` drops the link's name
/// without ever consulting where it points — Linux walks `..` physically and
/// lands elsewhere when `link` crosses directories. The divergence predates
/// this walker; nothing on either kernel reaches it.
///
/// One allocation per component (`probe`) and one per hop. The previous
/// resolver's shape — one probe total, of the finished path — is precisely
/// what made it wrong; every probe is a filesystem lookup regardless, and the
/// caller pays it in ext2's in-memory cache.
///
/// A splice rebuilds the whole remaining path and restarts the walk at `/`,
/// so components ahead of the link are probed again. The link's own name is
/// gone from the text by then (`canonicalize_path` spliced the target in),
/// so every splice strictly shortens the symlink work left; `max_hops` is
/// what turns a link-to-`.` (a splice that never shortens anything) into an
/// ELOOP-shaped stop instead of a hang.
#[must_use]
pub fn resolve_symlink_chain<F>(path: &str, max_hops: usize, mut read_link: F) -> String
where
    F: FnMut(&str) -> Option<String>,
{
    // `resolved` — the consumed prefix, always `/a/b` with no trailing slash.
    // `rest` — the components still to consume, canonical form (`/a/b`, or
    // empty). Together they are the walk's whole state.
    let mut resolved = String::from("/");
    let mut rest = canonicalize_path(path);
    let mut hops = 0usize;

    loop {
        if rest.len() <= 1 {
            // Empty, or just "/": nothing left to consume.
            break;
        }
        // `rest` is non-empty and starts with `/`; the first component ends
        // at the next slash (or at the end). `'/'` is ASCII, so byte indices
        // are safe to slice on.
        let end = rest[1..].find('/').map_or(rest.len(), |i| 1 + i);
        let comp = &rest[1..end];

        // Joining onto the root must not produce a double slash, but must
        // keep the leading one: `/` + `opt` is `/opt`, not `opt`.
        let mut probe = String::with_capacity(resolved.len() + comp.len() + 2);
        if resolved.len() > 1 {
            probe.push_str(&resolved);
        }
        probe.push('/');
        probe.push_str(comp);

        if let Some(target) = read_link(&probe) {
            hops += 1;
            if hops > max_hops {
                // ELOOP. Stop at the link itself; the caller's open then
                // fails on it the way a too-deep chain deserves to.
                return probe;
            }
            // Rebuild everything still to walk: the link's directory (the
            // consumed prefix) + the target + whatever came after the
            // link. A relative target joins against the link's directory
            // because that is the prefix we kept; an absolute target must
            // not keep it — the pushed base is skipped and the target's
            // own leading slash restarts the path at `/`.
            let tail = &rest[end..]; // "" or "/more/components"
            let absolute = target.starts_with('/');
            let mut full =
                String::with_capacity(resolved.len() + target.len() + tail.len() + 2);
            if !absolute && resolved.len() > 1 {
                full.push_str(&resolved);
            }
            full.push('/');
            full.push_str(&target);
            full.push_str(tail);
            resolved.truncate(1);
            rest = canonicalize_path(&full);
        } else {
            // Not a link (or not there at all — the caller's open fails
            // on that later, the same as before). Consume the component
            // and its leading slash; `rest` stays canonical.
            resolved = probe;
            rest.drain(..end);
        }
    }
    resolved
}

/// Split a path into components, without allocating.
///
/// Returns an iterator rather than a `Vec`: every caller either loops or
/// `collect`s on its own terms, and the vector was pure overhead.
pub fn path_components(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|s| !s.is_empty())
}

/// Place an in-root absolute path under a process's `chroot` directory.
///
/// `root` is the process's root as a **real** absolute path (`/` when it has
/// never called `chroot`, in which case this is `None` and the caller keeps its
/// path — the common case allocates nothing here). `absolute` must already be
/// canonical (`resolve_path`'s output): its `..` were folded against `/` of the
/// *chroot*, which is the whole confinement — `/../../etc` is `/etc` before it
/// is ever joined, so it can only name something under `root`.
#[must_use]
pub fn under_root(root: &str, absolute: &str) -> Option<String> {
    if root == "/" {
        return None;
    }
    let mut out = String::with_capacity(root.len() + absolute.len());
    out.push_str(root);
    if absolute != "/" {
        out.push_str(absolute);
    }
    Some(out)
}

#[cfg(test)]
mod under_root_tests {
    use super::*;

    #[test]
    fn no_chroot_changes_nothing() {
        assert_eq!(under_root("/", "/etc/passwd"), None);
        assert_eq!(under_root("/", "/"), None);
    }

    #[test]
    fn a_path_lands_under_the_root() {
        assert_eq!(under_root("/var/empty", "/etc").as_deref(), Some("/var/empty/etc"));
        assert_eq!(under_root("/var/empty", "/").as_deref(), Some("/var/empty"));
    }

    #[test]
    fn dotdot_cannot_climb_out() {
        // The walk folds `..` against the chroot's own `/` first, so the join
        // only ever sees a path that is already inside.
        let inside = resolve_path("/", "/../../etc/shadow");
        assert_eq!(inside, "/etc/shadow");
        assert_eq!(under_root("/jail", &inside).as_deref(), Some("/jail/etc/shadow"));
        let rel = resolve_path("/a", "../../../x");
        assert_eq!(under_root("/jail", &rel).as_deref(), Some("/jail/x"));
    }

    #[test]
    fn nested_chroots_compose_by_joining() {
        // chroot("/a") then chroot("/b") inside it: the second argument is read
        // in the first's view, so the real root is /a/b.
        let first = "/a";
        let second_in_view = "/b";
        let real = under_root(first, second_in_view).unwrap();
        assert_eq!(real, "/a/b");
        assert_eq!(under_root(&real, "/f").as_deref(), Some("/a/b/f"));
    }
}
