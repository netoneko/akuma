use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use akuma_vfs::{canonicalize_path, DirEntry, Filesystem, FsError, FsStats, Metadata, FS_MAX_PATH_SIZE};

/// Clamp `path` to the subtree root, resolving `.` and `..` first.
///
/// The prefix concatenation below is the whole of a box's chroot-style jail, so
/// a path that walks off the top with `..` has to land back on the virtual root
/// rather than ascending into the host filesystem — `docs/archive/BOX_CONTAINERS.md`
/// calls this out as a safety requirement. Kernel callers already normalize
/// before they get here (`with_fs` → `resolve_path` in `src/vfs/mod.rs`), so this
/// is the second lock on the door; for an already-canonical path it allocates
/// nothing and the stack-buffer fast path below is untouched.
fn confine(path: &str) -> Option<String> {
    if !path.split('/').any(|c| c == "." || c == "..") {
        return None;
    }
    Some(canonicalize_path(path))
}

/// Concatenate `$prefix` and `$path` into a stack buffer, binding the
/// result as `$name: &str`. Falls back to a heap `String` only when the
/// combined length exceeds `FS_MAX_PATH_SIZE`.
macro_rules! full_path {
    ($name:ident, $prefix:expr, $path:expr) => {
        let prefix: &str = $prefix;
        let path: &str = $path;
        let _confined = confine(path);
        let path: &str = _confined.as_deref().unwrap_or(path);
        let (need, is_root) = if path == "/" {
            (prefix.len(), true)
        } else {
            (prefix.len() + path.len(), false)
        };

        let mut _stack_buf = [0u8; FS_MAX_PATH_SIZE];
        let _heap_buf: String;

        let $name: &str = if need <= FS_MAX_PATH_SIZE {
            let buf = &mut _stack_buf[..need];
            buf[..prefix.len()].copy_from_slice(prefix.as_bytes());
            if !is_root {
                buf[prefix.len()..].copy_from_slice(path.as_bytes());
            }
            // Valid UTF-8 by construction: `buf` is exactly two `&str`s
            // concatenated. `from_utf8_unchecked` only skipped the validation
            // pass, and that pass is a walk over a path of a few tens of bytes
            // — far too cheap to buy an `unsafe` block with, and dropping it is
            // what lets this crate carry `#![forbid(unsafe_code)]`. The `Err`
            // arm is unreachable; it yields an empty path, which fails the
            // lookup that follows rather than fabricating one.
            core::str::from_utf8(&buf[..need]).unwrap_or("")
        } else {
            _heap_buf = if is_root {
                String::from(prefix)
            } else {
                let mut s = String::with_capacity(need);
                s.push_str(prefix);
                s.push_str(path);
                s
            };
            &_heap_buf
        };
    };
}

/// A filesystem view scoped to a subdirectory of an existing filesystem.
///
/// All path operations are transparently prefixed with a base path,
/// making a subdirectory appear as the root. This replaces the old
/// `root_dir` prefix hack with a proper `Filesystem` implementation
/// that the mount table can use directly.
pub struct SubdirFs {
    inner: Arc<dyn Filesystem>,
    prefix: String,
}

impl SubdirFs {
    #[must_use]
    pub fn new(inner: Arc<dyn Filesystem>, prefix: &str) -> Self {
        let prefix = prefix.trim_end_matches('/');
        Self {
            inner,
            prefix: String::from(prefix),
        }
    }
}

impl Filesystem for SubdirFs {
    fn name(&self) -> &'static str {
        "subdirfs"
    }

    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        full_path!(p, &self.prefix, path);
        self.inner.read_dir(p)
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>, FsError> {
        full_path!(p, &self.prefix, path);
        self.inner.read_file(p)
    }

    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        full_path!(p, &self.prefix, path);
        self.inner.write_file(p, data)
    }


    fn read_at(&self, path: &str, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
        full_path!(p, &self.prefix, path);
        self.inner.read_at(p, offset, buf)
    }

    fn write_at(&self, path: &str, offset: usize, data: &[u8]) -> Result<usize, FsError> {
        full_path!(p, &self.prefix, path);
        self.inner.write_at(p, offset, data)
    }

    fn create_dir(&self, path: &str) -> Result<(), FsError> {
        full_path!(p, &self.prefix, path);
        self.inner.create_dir(p)
    }

    fn remove_file(&self, path: &str) -> Result<(), FsError> {
        full_path!(p, &self.prefix, path);
        self.inner.remove_file(p)
    }

    fn remove_dir(&self, path: &str) -> Result<(), FsError> {
        full_path!(p, &self.prefix, path);
        self.inner.remove_dir(p)
    }

    fn exists(&self, path: &str) -> bool {
        full_path!(p, &self.prefix, path);
        self.inner.exists(p)
    }

    fn metadata(&self, path: &str) -> Result<Metadata, FsError> {
        full_path!(p, &self.prefix, path);
        self.inner.metadata(p)
    }

    fn create_symlink(&self, link_path: &str, target: &str) -> Result<(), FsError> {
        full_path!(p, &self.prefix, link_path);
        self.inner.create_symlink(p, target)
    }

    fn read_symlink(&self, path: &str) -> Result<String, FsError> {
        full_path!(p, &self.prefix, path);
        self.inner.read_symlink(p)
    }

    fn is_symlink(&self, path: &str) -> bool {
        full_path!(p, &self.prefix, path);
        self.inner.is_symlink(p)
    }

    fn chmod(&self, path: &str, mode: u32) -> Result<(), FsError> {
        full_path!(p, &self.prefix, path);
        self.inner.chmod(p, mode)
    }

    fn rename(&self, old_path: &str, new_path: &str) -> Result<(), FsError> {
        full_path!(old, &self.prefix, old_path);
        full_path!(new, &self.prefix, new_path);
        self.inner.rename(old, new)
    }

    fn stats(&self) -> Result<FsStats, FsError> {
        self.inner.stats()
    }

    fn sync(&self) -> Result<(), FsError> {
        self.inner.sync()
    }

    fn resolve_inode(&self, path: &str) -> Result<u32, FsError> {
        full_path!(p, &self.prefix, path);
        self.inner.resolve_inode(p)
    }

    fn read_at_by_inode(&self, inode: u32, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
        self.inner.read_at_by_inode(inode, offset, buf)
    }

    /// Forwarded unchanged, like `read_at_by_inode`: an inode number is already
    /// the inner filesystem's, so there is no prefix to apply. The `SubdirFs`
    /// jail is enforced where paths are — a caller only obtains this number by
    /// resolving a path through this instance in the first place.
    fn metadata_by_inode(&self, inode: u32) -> Result<Metadata, FsError> {
        self.inner.metadata_by_inode(inode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spinning_top::Spinlock;

    /// Records the paths the wrapped filesystem is actually asked for, which is
    /// the only thing that matters for confinement: the box escapes exactly when
    /// a path outside `prefix` reaches the inner filesystem.
    struct Recorder {
        seen: Spinlock<Vec<String>>,
    }

    impl Recorder {
        fn new() -> Arc<Self> {
            Arc::new(Self { seen: Spinlock::new(Vec::new()) })
        }

        fn record(&self, path: &str) {
            self.seen.lock().push(String::from(path));
        }

        fn last(&self) -> String {
            self.seen.lock().last().cloned().unwrap_or_default()
        }
    }

    impl Filesystem for Recorder {
        fn name(&self) -> &str {
            "recorder"
        }
        fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, FsError> {
            self.record(path);
            Ok(Vec::new())
        }
        fn read_file(&self, path: &str) -> Result<Vec<u8>, FsError> {
            self.record(path);
            Ok(Vec::new())
        }
        fn write_file(&self, path: &str, _data: &[u8]) -> Result<(), FsError> {
            self.record(path);
            Ok(())
        }
        fn create_dir(&self, path: &str) -> Result<(), FsError> {
            self.record(path);
            Ok(())
        }
        fn remove_file(&self, path: &str) -> Result<(), FsError> {
            self.record(path);
            Ok(())
        }
        fn remove_dir(&self, path: &str) -> Result<(), FsError> {
            self.record(path);
            Ok(())
        }
        fn exists(&self, path: &str) -> bool {
            self.record(path);
            true
        }
        fn metadata(&self, path: &str) -> Result<Metadata, FsError> {
            self.record(path);
            Err(FsError::NotFound)
        }
        fn rename(&self, old_path: &str, new_path: &str) -> Result<(), FsError> {
            self.record(old_path);
            self.record(new_path);
            Ok(())
        }
        fn create_symlink(&self, link_path: &str, _target: &str) -> Result<(), FsError> {
            self.record(link_path);
            Ok(())
        }
        fn stats(&self) -> Result<FsStats, FsError> {
            Err(FsError::NotSupported)
        }
    }

    fn jail() -> (Arc<Recorder>, SubdirFs) {
        let rec = Recorder::new();
        let fs = SubdirFs::new(rec.clone(), "/srv/rumpbox");
        (rec, fs)
    }

    #[test]
    fn plain_paths_are_prefixed_unchanged() {
        let (rec, fs) = jail();
        let _ = fs.read_file("/etc/passwd");
        assert_eq!(rec.last(), "/srv/rumpbox/etc/passwd");
        let _ = fs.read_dir("/");
        assert_eq!(rec.last(), "/srv/rumpbox");
    }

    #[test]
    fn dotdot_cannot_ascend_past_the_virtual_root() {
        let (rec, fs) = jail();
        // The classic escape: without confinement this reaches the host's
        // /etc/passwd via "/srv/rumpbox/../../etc/passwd".
        let _ = fs.read_file("/../../etc/passwd");
        assert_eq!(rec.last(), "/srv/rumpbox/etc/passwd");

        let _ = fs.read_file("/a/b/../../../../../etc/shadow");
        assert_eq!(rec.last(), "/srv/rumpbox/etc/shadow");

        // A `..` that stays inside still resolves normally.
        let _ = fs.read_file("/a/b/../c");
        assert_eq!(rec.last(), "/srv/rumpbox/a/c");
    }

    #[test]
    fn dotdot_that_lands_on_the_root_maps_to_the_prefix() {
        let (rec, fs) = jail();
        let _ = fs.read_dir("/..");
        assert_eq!(rec.last(), "/srv/rumpbox");
        let _ = fs.read_dir("/etc/..");
        assert_eq!(rec.last(), "/srv/rumpbox");
    }

    #[test]
    fn single_dot_components_are_resolved() {
        let (rec, fs) = jail();
        let _ = fs.read_file("/./etc/./passwd");
        assert_eq!(rec.last(), "/srv/rumpbox/etc/passwd");
    }

    #[test]
    fn every_path_taking_method_is_confined() {
        let (rec, fs) = jail();
        let escape = "/../../etc/passwd";
        let expected = "/srv/rumpbox/etc/passwd";

        let mut buf = [0u8; 4];
        macro_rules! check {
            ($label:literal, $call:expr) => {
                let _ = $call;
                assert_eq!(rec.last(), expected, concat!($label, " let a path out of the jail"));
            };
        }

        check!("read_dir", fs.read_dir(escape));
        check!("read_file", fs.read_file(escape));
        check!("write_file", fs.write_file(escape, b"x"));
        check!("read_at", fs.read_at(escape, 0, &mut buf));
        check!("write_at", fs.write_at(escape, 0, b"x"));
        check!("create_dir", fs.create_dir(escape));
        check!("remove_file", fs.remove_file(escape));
        check!("remove_dir", fs.remove_dir(escape));
        check!("exists", fs.exists(escape));
        check!("metadata", fs.metadata(escape));
        check!("create_symlink", fs.create_symlink(escape, "/tmp"));
        check!("read_symlink", fs.read_symlink(escape));
        check!("is_symlink", fs.is_symlink(escape));
        check!("chmod", fs.chmod(escape, 0o644));
        check!("resolve_inode", fs.resolve_inode(escape));
    }

    #[test]
    fn rename_confines_both_operands() {
        let rec = Recorder::new();
        let fs = SubdirFs::new(rec.clone(), "/srv/rumpbox");
        let _ = fs.rename("/../../etc/passwd", "/../../etc/shadow");
        let seen = rec.seen.lock().clone();
        assert_eq!(seen, alloc::vec![
            String::from("/srv/rumpbox/etc/passwd"),
            String::from("/srv/rumpbox/etc/shadow"),
        ]);
    }

    #[test]
    fn long_paths_take_the_heap_fallback_and_stay_confined() {
        let rec = Recorder::new();
        let fs = SubdirFs::new(rec.clone(), "/srv/rumpbox");
        // Overflow FS_MAX_PATH_SIZE so the macro's String branch runs.
        let mut deep = String::new();
        for _ in 0..(FS_MAX_PATH_SIZE / 4 + 8) {
            deep.push_str("/aaa");
        }
        deep.push_str("/../../../../etc/passwd");
        let _ = fs.read_file(&deep);
        let got = rec.last();
        assert!(got.starts_with("/srv/rumpbox/"), "escaped the jail: {got}");
        assert!(got.ends_with("/etc/passwd"), "unexpected tail: {got}");
        assert!(!got.contains(".."), "unresolved traversal survived: {got}");
    }
}
