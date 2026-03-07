use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use akuma_vfs::{DirEntry, Filesystem, FsError, FsStats, Metadata, FS_MAX_PATH_SIZE};

/// Concatenate `$prefix` and `$path` into a stack buffer, binding the
/// result as `$name: &str`. Falls back to a heap `String` only when the
/// combined length exceeds `FS_MAX_PATH_SIZE`.
macro_rules! full_path {
    ($name:ident, $prefix:expr, $path:expr) => {
        let prefix: &str = $prefix;
        let path: &str = $path;
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
            unsafe { core::str::from_utf8_unchecked(&buf[..need]) }
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

    fn append_file(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        full_path!(p, &self.prefix, path);
        self.inner.append_file(p, data)
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
}
