//! VFS types and the `Filesystem` trait.

use alloc::string::String;
use alloc::vec::Vec;

/// Filesystem error type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    BlockDeviceNotInitialized,
    NotInitialized,
    NotFound,
    PermissionDenied,
    AlreadyExists,
    NotADirectory,
    NotAFile,
    DirectoryNotEmpty,
    IoError,
    InvalidPath,
    NoSpace,
    TooManyOpenFiles,
    InvalidHandle,
    Corrupt,
    EndOfFile,
    NoFilesystem,
    Internal,
    ReadOnly,
    NotSupported,
}

impl core::fmt::Display for FsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let msg = match self {
            Self::BlockDeviceNotInitialized => "Block device not initialized",
            Self::NotInitialized => "Filesystem not initialized",
            Self::NotFound => "Not found",
            Self::PermissionDenied => "Permission denied",
            Self::AlreadyExists => "Already exists",
            Self::NotADirectory => "Not a directory",
            Self::NotAFile => "Not a file",
            Self::DirectoryNotEmpty => "Directory not empty",
            Self::IoError => "I/O error",
            Self::InvalidPath => "Invalid path",
            Self::NoSpace => "No space left",
            Self::TooManyOpenFiles => "Too many open files",
            Self::InvalidHandle => "Invalid file handle",
            Self::Corrupt => "Filesystem corrupt",
            Self::EndOfFile => "End of file",
            Self::NoFilesystem => "No filesystem found",
            Self::Internal => "Internal error",
            Self::ReadOnly => "Read-only filesystem",
            Self::NotSupported => "Operation not supported",
        };
        f.write_str(msg)
    }
}

/// Directory entry information
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
}

/// File or directory metadata
#[derive(Debug, Clone)]
pub struct Metadata {
    pub is_dir: bool,
    pub size: u64,
    pub inode: u64,
    /// File mode (type + permissions), e.g. `0o100755` for executable file
    pub mode: u32,
    pub created: Option<u64>,
    pub modified: Option<u64>,
    pub accessed: Option<u64>,
}

/// Filesystem statistics
#[derive(Debug, Clone)]
pub struct FsStats {
    pub block_size: u32,
    pub total_blocks: u64,
    pub free_blocks: u64,
}

impl FsStats {
    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.total_blocks * self.block_size as u64
    }

    #[must_use]
    pub const fn free_bytes(&self) -> u64 {
        self.free_blocks * self.block_size as u64
    }

    #[must_use]
    pub const fn used_bytes(&self) -> u64 {
        self.total_bytes() - self.free_bytes()
    }
}

/// Information about a mounted filesystem
#[derive(Debug, Clone)]
pub struct MountInfo {
    pub path: String,
    pub fs_type: String,
}

/// Trait for filesystem implementations (object-safe).
pub trait Filesystem: Send + Sync {
    fn name(&self) -> &str;
    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, FsError>;
    fn read_file(&self, path: &str) -> Result<Vec<u8>, FsError>;
    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), FsError>;

    fn read_at(&self, path: &str, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
        let data = self.read_file(path)?;
        if offset >= data.len() {
            return Ok(0);
        }
        let n = buf.len().min(data.len() - offset);
        buf[..n].copy_from_slice(&data[offset..offset + n]);
        Ok(n)
    }

    fn write_at(&self, path: &str, offset: usize, data: &[u8]) -> Result<usize, FsError> {
        let mut contents = self.read_file(path).unwrap_or_default();
        let end = offset + data.len();
        if end > contents.len() {
            contents.resize(end, 0);
        }
        contents[offset..end].copy_from_slice(data);
        self.write_file(path, &contents)?;
        Ok(data.len())
    }

    fn create_dir(&self, path: &str) -> Result<(), FsError>;
    fn remove_file(&self, path: &str) -> Result<(), FsError>;
    fn remove_dir(&self, path: &str) -> Result<(), FsError>;
    fn exists(&self, path: &str) -> bool;
    fn metadata(&self, path: &str) -> Result<Metadata, FsError>;

    fn create_symlink(&self, _link_path: &str, _target: &str) -> Result<(), FsError> {
        Err(FsError::NotSupported)
    }

    /// Create a zero-length socket node (`S_IFSOCK`) — the filesystem presence
    /// of an AF_UNIX `bind(2)` on a pathname.
    ///
    /// Defaults to `NotSupported` so a filesystem that cannot represent the
    /// type says so, rather than silently creating a regular file. That
    /// substitution is exactly the bug this method exists to fix: `stat`
    /// reported `S_IFREG`, and a client checking `S_ISSOCK` before connecting
    /// refused to talk to a working socket
    /// (`docs/archive/UNIX_SOCKET_IMPROVEMENTS.md` G7).
    fn create_socket_node(&self, _path: &str) -> Result<(), FsError> {
        Err(FsError::NotSupported)
    }

    fn read_symlink(&self, _path: &str) -> Result<String, FsError> {
        Err(FsError::NotFound)
    }

    fn is_symlink(&self, _path: &str) -> bool {
        false
    }

    fn chmod(&self, _path: &str, _mode: u32) -> Result<(), FsError> {
        Err(FsError::NotSupported)
    }

    fn truncate(&self, _path: &str, _length: u64) -> Result<(), FsError> {
        Err(FsError::NotSupported)
    }

    fn fallocate(&self, _path: &str, _mode: i32, _offset: u64, _len: u64) -> Result<(), FsError> {
        Err(FsError::NotSupported)
    }

    fn rename(&self, _old_path: &str, _new_path: &str) -> Result<(), FsError> {
        Err(FsError::NotSupported)
    }

    fn stats(&self) -> Result<FsStats, FsError>;

    fn sync(&self) -> Result<(), FsError> {
        Ok(())
    }

    fn resolve_inode(&self, _path: &str) -> Result<u32, FsError> {
        Err(FsError::NotSupported)
    }

    fn read_at_by_inode(
        &self,
        _inode: u32,
        _offset: usize,
        _buf: &mut [u8],
    ) -> Result<usize, FsError> {
        Err(FsError::NotSupported)
    }

    /// [`Filesystem::metadata`] for a caller that already holds the inode
    /// number — the `stat` half of [`Filesystem::read_at_by_inode`].
    ///
    /// An open fd carries the inode `open(2)` resolved (`KernelFile::inode`), so
    /// `fstat`, `statx(AT_EMPTY_PATH)` and `lseek(SEEK_END)` can answer without
    /// walking the directory tree — and, more importantly, can still answer once
    /// the fd's *name* is gone. Without this they resolve `f.path` and fail on an
    /// unlinked-but-open fd whose `read` works perfectly well.
    ///
    /// Defaults to `NotSupported`, so a filesystem with no inode addressing keeps
    /// serving those syscalls by path exactly as before.
    fn metadata_by_inode(&self, _inode: u32) -> Result<Metadata, FsError> {
        Err(FsError::NotSupported)
    }
}
