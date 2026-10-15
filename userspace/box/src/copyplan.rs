//! Path arithmetic for `COPY`/`ADD` — where a source lands. Pure, host-tested.

use alloc::string::String;
use alloc::vec::Vec;

/// Make `p` absolute against `workdir` and clean it (`.`, `..`, `//`).
/// `..` cannot climb above `/`.
pub fn absolutize(workdir: &str, p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let joined_workdir = if p.starts_with('/') { "" } else { workdir };
    for seg in joined_workdir.split('/').chain(p.split('/')) {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    let mut out = String::from("/");
    out.push_str(&parts.join("/"));
    out
}

/// A source inside the build context, or `None` if it would escape it.
/// Leading `/` and `./` are tolerated (Docker treats them as context-relative).
pub fn context_relative(src: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for seg in src.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

/// The destination of a `COPY`/`ADD` and whether it names a directory.
///
/// A destination is a directory when it ends in `/`, when there are several
/// sources, or when the single source is itself a directory (its *contents* are
/// copied into it). Otherwise a single file lands *at* the destination path.
#[derive(Debug, PartialEq, Eq)]
pub struct Dest {
    pub path: String,
    pub is_dir: bool,
}

pub fn plan_dest(workdir: &str, dest: &str, n_srcs: usize) -> Dest {
    let trailing = dest.ends_with('/') || dest == "." || dest.ends_with("/.");
    Dest { path: absolutize(workdir, dest), is_dir: trailing || n_srcs > 1 }
}

/// Where one file `name` (the source's last component, or a path relative to a
/// copied directory) ends up.
pub fn target_for(dest: &Dest, src_is_dir: bool, relative: &str, src_name: &str) -> String {
    if src_is_dir {
        // Contents of the directory go into dest.
        if relative.is_empty() {
            dest.path.clone()
        } else {
            join(&dest.path, relative)
        }
    } else if dest.is_dir {
        join(&dest.path, src_name)
    } else {
        dest.path.clone()
    }
}

fn join(a: &str, b: &str) -> String {
    if a == "/" {
        alloc::format!("/{b}")
    } else {
        alloc::format!("{a}/{b}")
    }
}

/// Parse `--chmod`: octal, 3 or 4 digits.
pub fn parse_chmod(s: &str) -> Option<u32> {
    if s.is_empty() || s.len() > 4 || !s.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return None;
    }
    u32::from_str_radix(s, 8).ok()
}

/// Whether `src` is something `ADD` fetches rather than reads from the context.
pub fn is_url(src: &str) -> bool {
    src.starts_with("http://") || src.starts_with("https://")
}

/// Whether `ADD` should unpack this local file.
pub fn is_unpackable_archive(name: &str) -> bool {
    name.ends_with(".tar") || name.ends_with(".tar.gz") || name.ends_with(".tgz")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolutize_cases() {
        assert_eq!(absolutize("/app", "x"), "/app/x");
        assert_eq!(absolutize("/app", "/x"), "/x");
        assert_eq!(absolutize("/app", "../x"), "/x");
        assert_eq!(absolutize("/", "../../x"), "/x");
        assert_eq!(absolutize("/app", "."), "/app");
        assert_eq!(absolutize("/app", "a//b/./c/"), "/app/a/b/c");
        assert_eq!(absolutize("", "x"), "/x");
    }

    #[test]
    fn context_paths_cannot_escape() {
        assert_eq!(context_relative("src/main.rs").as_deref(), Some("src/main.rs"));
        assert_eq!(context_relative("./a/../b").as_deref(), Some("b"));
        assert_eq!(context_relative("/abs").as_deref(), Some("abs"));
        assert_eq!(context_relative(".").as_deref(), Some(""));
        assert_eq!(context_relative("../secret"), None);
        assert_eq!(context_relative("a/../../secret"), None);
    }

    #[test]
    fn destination_directory_rules() {
        assert!(plan_dest("/", "/opt/", 1).is_dir);
        assert!(plan_dest("/", "/opt", 2).is_dir);
        assert!(!plan_dest("/", "/opt/file", 1).is_dir);
        assert!(plan_dest("/app", ".", 1).is_dir);
        assert_eq!(plan_dest("/app", "bin/", 1).path, "/app/bin");
    }

    #[test]
    fn targets() {
        let dir = plan_dest("/", "/opt/", 1);
        assert_eq!(target_for(&dir, false, "", "tool"), "/opt/tool");
        assert_eq!(target_for(&dir, true, "", "src"), "/opt");
        assert_eq!(target_for(&dir, true, "sub/f", "src"), "/opt/sub/f");
        let file = plan_dest("/", "/opt/renamed", 1);
        assert_eq!(target_for(&file, false, "", "tool"), "/opt/renamed");
        let root = plan_dest("/", "/", 1);
        assert_eq!(target_for(&root, false, "", "f"), "/f");
        assert_eq!(target_for(&root, true, "d/f", "x"), "/d/f");
    }

    #[test]
    fn chmod_parsing() {
        assert_eq!(parse_chmod("755"), Some(0o755));
        assert_eq!(parse_chmod("0644"), Some(0o644));
        assert_eq!(parse_chmod("u+x"), None);
        assert_eq!(parse_chmod("888"), None);
        assert_eq!(parse_chmod(""), None);
    }

    #[test]
    fn add_source_kinds() {
        assert!(is_url("https://x/y.tgz"));
        assert!(!is_url("y.tgz"));
        assert!(is_unpackable_archive("a.tar.gz") && is_unpackable_archive("a.tgz") && is_unpackable_archive("a.tar"));
        assert!(!is_unpackable_archive("a.zip"));
    }
}
