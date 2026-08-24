//! Mount set — maps paths to filesystem implementations.
//!
//! One implementation, two capacities. The kernel's global table
//! ([`MountTable`] = `MountSet<8>`) and a container's mount namespace
//! (`akuma_isolation::MountNamespace` = `MountSet<16>`) were separate 200- and
//! 300-line types with byte-identical `mount` / `unmount` / `resolve` /
//! `resolve_arc` / `list_mounts` / `child_mount_points`, differing only in the
//! capacity constant and in whether they called [`normalize_mount_path`] or
//! inlined it (the namespace inlined the same two lines three times).
//!
//! `TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §4 flagged this as the cross-crate
//! clone that "should worry you most", because `akuma-vfs` is the leaf
//! `akuma-isolation` depends on: the shared half belonged here and nothing was
//! stopping it.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::types::{DirEntry, Filesystem, FsError, MountInfo};

/// `MS_RDONLY` — the only mount flag the kernel stores and honours today.
pub const MS_RDONLY: u64 = 1;
/// `MS_REMOUNT` — `mount(2)` flags bit selecting the remount path.
pub const MS_REMOUNT: u64 = 32;
/// `ST_RDONLY` — the `statfs`/`fstatfs` `f_flags` bit for a read-only mount.
pub const ST_RDONLY: u64 = 1;

struct MountEntry {
    path: String,
    /// Device/source this fs was mounted from (`/dev/vda`, `proc`, …), if the
    /// mount came from a source that had one. Boot mounts and `MOUNT_IN_NS`
    /// set it; a plain `mount("tmpfs")` leaves it `None`.
    source: Option<String>,
    /// Stored `MS_*` bits. Only `MS_RDONLY` is honoured (enforced at the
    /// kernel's VFS write chokepoints); the rest of `mount(2)`'s flag word is
    /// accepted and dropped.
    flags: u64,
    fs: Arc<dyn Filesystem>,
}

/// A set of mounted filesystems, holding at most `MAX` of them.
///
/// Not global — the kernel owns the [`MountTable`] singleton and provides
/// process-aware path resolution on top, and each container owns a
/// `MountNamespace`.
///
/// Longest mount path wins: `mount` keeps `mounts` sorted by descending path
/// length, so `resolve` returns the deepest match by scanning in order.
pub struct MountSet<const MAX: usize> {
    mounts: Vec<MountEntry>,
}

impl<const MAX: usize> MountSet<MAX> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            mounts: Vec::with_capacity(MAX),
        }
    }

    /// Mount `fs` at `path` with no source and no flags.
    ///
    /// # Errors
    /// `NoSpace` past `MAX` mounts; `AlreadyExists` if `path` is already a mount
    /// point (mounts do not stack — see [`Self::replace_pristine_root`] for the
    /// one case that needs a swap).
    pub fn mount(&mut self, path: &str, fs: Arc<dyn Filesystem>) -> Result<(), FsError> {
        self.mount_with(path, None, 0, fs)
    }

    /// Mount `fs` at `path`, recording `source` and `flags`.
    ///
    /// The rest of the contract is [`Self::mount`]'s.
    ///
    /// # Errors
    /// `NoSpace` past `MAX` mounts; `AlreadyExists` if `path` is already a
    /// mount point.
    pub fn mount_with(
        &mut self,
        path: &str,
        source: Option<&str>,
        flags: u64,
        fs: Arc<dyn Filesystem>,
    ) -> Result<(), FsError> {
        if self.mounts.len() >= MAX {
            return Err(FsError::NoSpace);
        }
        if self.mounts.iter().any(|m| m.path == path) {
            return Err(FsError::AlreadyExists);
        }
        self.mounts.push(MountEntry {
            path: String::from(path),
            source: source.map(String::from),
            flags: flags & MS_RDONLY,
            fs,
        });
        self.mounts.sort_by(|a, b| b.path.len().cmp(&a.path.len()));
        Ok(())
    }

    /// Replace the stored flags of the mount at `path` (`MS_REMOUNT` leg of
    /// `mount(2)`). Only the `MS_RDONLY` bit is kept; everything else in
    /// `flags` is accepted and dropped.
    ///
    /// # Errors
    /// `NotFound` if nothing is mounted at `path`.
    pub fn remount(&mut self, path: &str, flags: u64) -> Result<(), FsError> {
        let entry = self
            .mounts
            .iter_mut()
            .find(|m| m.path == path)
            .ok_or(FsError::NotFound)?;
        entry.flags = flags & MS_RDONLY;
        Ok(())
    }

    /// # Errors
    /// `NotFound` if nothing is mounted at `path`.
    pub fn unmount(&mut self, path: &str) -> Result<(), FsError> {
        let idx = self
            .mounts
            .iter()
            .position(|m| m.path == path)
            .ok_or(FsError::NotFound)?;
        self.mounts.remove(idx);
        Ok(())
    }

    /// Replace the root mount, but **only** while it is still the pristine one
    /// this set was born with.
    ///
    /// This exists for exactly one move: a box's namespace is created with a
    /// `SubdirFs` jail at `/` (`create_box_namespace`), and turning that root
    /// into an overlay is a swap, not a stack — [`Self::mount`] would reject the
    /// duplicate path.
    ///
    /// Swapping a root is dangerous in a way a plain mount is not: every path a
    /// running process resolves, and every relative lookup it has in flight,
    /// silently starts landing somewhere else. So this is written as a one-shot
    /// rather than a general "replace": it fails unless `/` currently holds a
    /// filesystem named `expected`, which is true only before anything has
    /// re-rooted the box. A second call cannot undo the first, and no caller can
    /// use it to point a live box's `/` at a directory of its choosing.
    ///
    /// The complementary rule lives in `umount2`, which refuses to let a box
    /// drop its own `/`. Between them, a box's root can be set once and never
    /// removed or redirected.
    ///
    /// **There is deliberately no unguarded `replace_root`.** This is the only
    /// path in the tree that writes an existing mount's `fs`, and the `expected`
    /// check is what makes it safe — so the guard travels with the operation
    /// rather than living in a wrapper that a future caller could bypass. That is
    /// also why it is available on the kernel's global [`MountTable`], which has
    /// no caller for it: exposing the *guarded* form costs nothing, whereas
    /// splitting the guard from the write would have meant publishing a bare
    /// setter for the namespace to build on.
    ///
    /// # Errors
    /// `PermissionDenied` if `/` is missing or is not `expected`.
    pub fn replace_pristine_root(
        &mut self,
        expected: &str,
        fs: Arc<dyn Filesystem>,
    ) -> Result<(), FsError> {
        let current = self
            .mounts
            .iter_mut()
            .find(|m| m.path == "/")
            .ok_or(FsError::PermissionDenied)?;
        if current.fs.name() != expected {
            return Err(FsError::PermissionDenied);
        }
        current.fs = fs;
        Ok(())
    }

    /// Resolve an **absolute** path to `(filesystem, relative_path)`.
    #[must_use]
    pub fn resolve<'a>(&'a self, path: &'a str) -> Option<(&'a dyn Filesystem, &'a str)> {
        let normalized = normalize_mount_path(path);

        for mount in &self.mounts {
            if mount.path == "/" {
                return Some((mount.fs.as_ref(), normalized));
            }
            if normalized == mount.path {
                return Some((mount.fs.as_ref(), "/"));
            }
            if normalized.starts_with(&mount.path[..]) {
                let rest = &normalized[mount.path.len()..];
                if rest.is_empty() {
                    return Some((mount.fs.as_ref(), "/"));
                }
                if rest.starts_with('/') {
                    return Some((mount.fs.as_ref(), rest));
                }
            }
        }

        None
    }

    /// Like [`Self::resolve`] but returns owned types so the lock can be released
    /// before I/O.
    #[must_use]
    pub fn resolve_arc(&self, path: &str) -> Option<(Arc<dyn Filesystem>, String)> {
        self.resolve_arc_with_flags(path).map(|(fs, rel, _flags)| (fs, rel))
    }

    /// [`Self::resolve_arc`] plus the resolved mount's stored `MS_*` flags —
    /// the kernel's read-only enforcement point needs to know which mount a
    /// path landed on.
    #[must_use]
    pub fn resolve_arc_with_flags(&self, path: &str) -> Option<(Arc<dyn Filesystem>, String, u64)> {
        let normalized = normalize_mount_path(path);

        for mount in &self.mounts {
            if mount.path == "/" {
                return Some((mount.fs.clone(), normalized.into(), mount.flags));
            }
            if normalized == mount.path {
                return Some((mount.fs.clone(), String::from("/"), mount.flags));
            }
            if normalized.starts_with(&mount.path[..]) {
                let rest = &normalized[mount.path.len()..];
                if rest.is_empty() {
                    return Some((mount.fs.clone(), String::from("/"), mount.flags));
                }
                if rest.starts_with('/') {
                    return Some((mount.fs.clone(), rest.into(), mount.flags));
                }
            }
        }

        None
    }

    /// Copy up to `out.len()` mounts into `out` as borrowed rows, returning the
    /// count written. The whole point is that **no allocation happens** — this
    /// runs under the set's spinlock, and the rows borrow straight out of the
    /// entries, so the lock must be held for the copy only and can be released
    /// before anything renders the rows.
    #[must_use]
    pub fn copy_mounts_into<'a>(&'a self, out: &mut [MountSnapshot<'a>]) -> usize {
        let n = out.len().min(self.mounts.len());
        for (slot, mount) in out.iter_mut().zip(self.mounts.iter()) {
            *slot = MountSnapshot {
                path: mount.path.as_str(),
                source: mount.source.as_deref(),
                fs_type: mount.fs.name(),
                flags: mount.flags,
            };
        }
        n
    }

    /// Visit every mount with a borrowed [`MountSnapshot`], in table order.
    ///
    /// This is the kernel-side listing primitive: the rows borrow from the
    /// set, so whatever uses them must run **inside** `f` — which in practice
    /// means while the caller's spinlock is still held. The contract is
    /// therefore the console rule's sibling: `f` must not allocate, block, or
    /// take another lock. Rendering into a caller-owned fixed buffer is the
    /// intended use (`docs/archive/MOUNT_MISSING_SYSCALLS.md` §2).
    pub fn for_each_mount(&self, mut f: impl FnMut(MountSnapshot<'_>)) {
        for mount in &self.mounts {
            f(MountSnapshot {
                path: mount.path.as_str(),
                source: mount.source.as_deref(),
                fs_type: mount.fs.name(),
                flags: mount.flags,
            });
        }
    }

    /// List all mounted filesystems.
    #[must_use]
    pub fn list_mounts(&self) -> Vec<MountInfo> {
        self.mounts
            .iter()
            .map(|m| MountInfo {
                path: m.path.clone(),
                fs_type: String::from(m.fs.name()),
            })
            .collect()
    }

    /// Mount points that are direct children of `parent_path`.
    #[must_use]
    pub fn child_mount_points(&self, parent_path: &str) -> Vec<DirEntry> {
        let parent = normalize_mount_path(parent_path);
        let mut entries = Vec::new();

        for mount in &self.mounts {
            if mount.path == "/" || mount.path == parent {
                continue;
            }
            let mount_path = mount.path.as_str();

            if parent == "/" {
                if mount_path.starts_with('/') && !mount_path[1..].contains('/') {
                    entries.push(child_entry(&mount_path[1..]));
                }
            } else {
                let prefix = format!("{parent}/");
                if mount_path.starts_with(&prefix) {
                    let rest = &mount_path[prefix.len()..];
                    if !rest.contains('/') {
                        entries.push(child_entry(rest));
                    }
                }
            }
        }

        entries
    }

    /// The `Arc<dyn Filesystem>` mounted at exactly `path`.
    #[must_use]
    pub fn get_fs(&self, path: &str) -> Option<Arc<dyn Filesystem>> {
        let normalized = normalize_mount_path(path);
        self.mounts.iter().find(|m| m.path == normalized).map(|m| m.fs.clone())
    }

    /// Sync all mounted filesystems.
    ///
    /// # Errors
    /// The first underlying `sync()` failure, which stops the sweep.
    pub fn sync_all(&self) -> Result<(), FsError> {
        for mount in &self.mounts {
            mount.fs.sync()?;
        }
        Ok(())
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.mounts.is_empty()
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.mounts.len()
    }
}

impl<const MAX: usize> Default for MountSet<MAX> {
    fn default() -> Self {
        Self::new()
    }
}

/// The kernel's global mount table.
pub type MountTable = MountSet<8>;

/// A borrowed, allocation-free view of one mount — what
/// [`MountSet::copy_mounts_into`] hands out under the lock.
#[derive(Clone, Copy, Debug)]
pub struct MountSnapshot<'a> {
    pub path: &'a str,
    pub source: Option<&'a str>,
    pub fs_type: &'a str,
    pub flags: u64,
}

impl MountSnapshot<'_> {
    /// The empty placeholder every snapshot array starts as (needs `Copy` for
    /// array initialization without allocation).
    pub const EMPTY: Self = Self {
        path: "",
        source: None,
        fs_type: "",
        flags: 0,
    };
}

fn child_entry(name: &str) -> DirEntry {
    DirEntry {
        name: String::from(name),
        is_dir: true,
        is_symlink: false,
        size: 0,
    }
}

/// Trim a path to the form mount points are compared against: no trailing slash,
/// and `""` becomes `"/"`.
///
/// **Not** a general path normaliser and deliberately non-allocating — it returns
/// a borrow so `resolve` can hand the caller a subslice of its own input. It does
/// not resolve `.` / `..` or add a leading slash; callers are expected to pass an
/// already-absolute path (the kernel's `vfs::with_fs` does that via
/// `path::resolve_path`). See `crate::path` for the allocating, fully-resolving
/// counterparts.
fn normalize_mount_path(path: &str) -> &str {
    let path = path.trim_end_matches('/');
    if path.is_empty() { "/" } else { path }
}
