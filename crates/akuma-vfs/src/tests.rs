#[cfg(test)]
mod path_tests {
    extern crate alloc;
    use alloc::vec;
    use alloc::vec::Vec;
    use crate::path::*;

    #[test]
    fn canonicalize_basic() {
        assert_eq!(canonicalize_path("/foo/bar"), "/foo/bar");
        assert_eq!(canonicalize_path("/foo/./bar"), "/foo/bar");
        assert_eq!(canonicalize_path("/foo/../bar"), "/bar");
        assert_eq!(canonicalize_path("/"), "/");
        assert_eq!(canonicalize_path(""), "/");
    }

    #[test]
    fn canonicalize_double_dots_at_root() {
        assert_eq!(canonicalize_path("/.."), "/");
        assert_eq!(canonicalize_path("/../foo"), "/foo");
    }

    #[test]
    fn resolve_path_absolute() {
        assert_eq!(resolve_path("/home", "/etc/passwd"), "/etc/passwd");
    }

    #[test]
    fn resolve_path_relative() {
        assert_eq!(resolve_path("/home/user", "docs/file.txt"), "/home/user/docs/file.txt");
        assert_eq!(resolve_path("/", "foo"), "/foo");
    }

    /// `resolve_path("/", p)` is what the kernel's `vfs::resolve_without_cwd`
    /// uses when there is no current process to take a CWD from. It replaced a
    /// local `normalize_path_owned` that trimmed trailing slashes and prepended a
    /// leading one but did **not** resolve `.` / `..` — so `..` was resolved when
    /// a process existed and left in the path when one did not, with both arms
    /// feeding the same `MountSet::resolve_arc`.
    ///
    /// The first group is where the two agreed; the second is the correction.
    #[test]
    fn resolve_path_at_root_is_the_no_cwd_normaliser() {
        // Unchanged from the old local helper.
        assert_eq!(resolve_path("/", ""), "/");
        assert_eq!(resolve_path("/", "/"), "/");
        assert_eq!(resolve_path("/", "/foo/"), "/foo");
        assert_eq!(resolve_path("/", "foo"), "/foo");
        assert_eq!(resolve_path("/", "/foo/bar"), "/foo/bar");

        // The correction: `..` and `.` are now resolved here too, so a path does
        // not mean two different things depending on whether a process is current.
        assert_eq!(resolve_path("/", "foo/../bar"), "/bar");
        assert_eq!(resolve_path("/", "./foo"), "/foo");
        assert_eq!(resolve_path("/", "foo/./bar/"), "/foo/bar");
        // Escaping above root clamps rather than producing a relative path.
        assert_eq!(resolve_path("/", "../../etc"), "/etc");
    }

    #[test]
    fn split_path_basic() {
        assert_eq!(split_path("/foo/bar/baz"), ("foo/bar", "baz"));
        assert_eq!(split_path("/single"), ("", "single"));
    }

    #[test]
    fn path_components_basic() {
        assert_eq!(path_components("/foo/bar/baz"), vec!["foo", "bar", "baz"]);
        assert_eq!(path_components("/"), Vec::<&str>::new());
        assert_eq!(path_components("//foo///bar//"), vec!["foo", "bar"]);
    }
}

#[cfg(test)]
 mod mount_tests {
     extern crate alloc;
     use alloc::sync::Arc;
#[allow(unused_imports)]
     use crate::{MountTable, MemoryFilesystem, MountSnapshot, Filesystem, FsError, MS_RDONLY};

    #[test]
    fn mount_and_resolve() {
        let mut mt = MountTable::new();
        mt.mount("/", Arc::new(MemoryFilesystem::new())).unwrap();
        let (fs, rel) = mt.resolve("/foo/bar").unwrap();
        assert_eq!(fs.name(), "memfs");
        assert_eq!(rel, "/foo/bar");
    }

    #[test]
    fn mount_nested() {
        let mut mt = MountTable::new();
        mt.mount("/", Arc::new(MemoryFilesystem::new())).unwrap();
        mt.mount("/tmp", Arc::new(MemoryFilesystem::new())).unwrap();
        let (fs, rel) = mt.resolve("/tmp/file").unwrap();
        assert_eq!(fs.name(), "memfs");
        assert_eq!(rel, "/file");
    }

    #[test]
    fn mount_duplicate_fails() {
        let mut mt = MountTable::new();
        mt.mount("/", Arc::new(MemoryFilesystem::new())).unwrap();
        let r = mt.mount("/", Arc::new(MemoryFilesystem::new()));
        assert!(r.is_err());
    }

    #[test]
    fn unmount() {
        let mut mt = MountTable::new();
        mt.mount("/", Arc::new(MemoryFilesystem::new())).unwrap();
        mt.mount("/tmp", Arc::new(MemoryFilesystem::new())).unwrap();
        mt.unmount("/tmp").unwrap();
        let (_, rel) = mt.resolve("/tmp/file").unwrap();
        assert_eq!(rel, "/tmp/file"); // falls through to root
    }

    #[test]
    fn child_mount_points() {
        let mut mt = MountTable::new();
        mt.mount("/", Arc::new(MemoryFilesystem::new())).unwrap();
        mt.mount("/proc", Arc::new(MemoryFilesystem::new())).unwrap();
        mt.mount("/tmp", Arc::new(MemoryFilesystem::new())).unwrap();
        let children = mt.child_mount_points("/");
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn list_mounts() {
        let mut mt = MountTable::new();
        mt.mount("/", Arc::new(MemoryFilesystem::new())).unwrap();
        mt.mount("/tmp", Arc::new(MemoryFilesystem::new())).unwrap();
        let mounts = mt.list_mounts();
        assert_eq!(mounts.len(), 2);
    }

    #[test]
    fn mount_with_records_source_and_rdonly_flag() {
        let mut mt = MountTable::new();
        mt.mount_with("/data", Some("/dev/vdb"), MS_RDONLY | 0x4000 /* ignored bits dropped */,
                      Arc::new(MemoryFilesystem::new())).unwrap();
        let mut rows = [MountSnapshot::EMPTY; 4];
        let n = mt.copy_mounts_into(&mut rows);
        assert_eq!(n, 1);
        assert_eq!(rows[0].path, "/data");
        assert_eq!(rows[0].source, Some("/dev/vdb"));
        assert_eq!(rows[0].fs_type, "memfs");
        assert_eq!(rows[0].flags, MS_RDONLY);
    }

    #[test]
    fn resolve_arc_with_flags_reports_resolved_mount() {
        let mut mt = MountTable::new();
        mt.mount_with("/", Some("/dev/vda"), 0, Arc::new(MemoryFilesystem::new())).unwrap();
        mt.mount_with("/ro", None, MS_RDONLY, Arc::new(MemoryFilesystem::new())).unwrap();
        let (_, _, flags_root) = mt.resolve_arc_with_flags("/etc/passwd").unwrap();
        let (_, _, flags_ro) = mt.resolve_arc_with_flags("/ro/file").unwrap();
        assert_eq!(flags_root, 0);
        assert_eq!(flags_ro, MS_RDONLY);
    }

    #[test]
    fn remount_flips_rdonly() {
        let mut mt = MountTable::new();
        mt.mount_with("/", Some("/dev/vda"), 0, Arc::new(MemoryFilesystem::new())).unwrap();
        mt.remount("/", MS_RDONLY).unwrap();
        assert_eq!(mt.resolve_arc_with_flags("/x").unwrap().2, MS_RDONLY);
        mt.remount("/", 0).unwrap();
        assert_eq!(mt.resolve_arc_with_flags("/x").unwrap().2, 0);
        assert!(matches!(mt.remount("/nope", 0), Err(FsError::NotFound)));
    }

    #[test]
    fn copy_mounts_into_truncates_to_buffer() {
        let mut mt = MountTable::new();
        mt.mount("/", Arc::new(MemoryFilesystem::new())).unwrap();
        mt.mount("/a", Arc::new(MemoryFilesystem::new())).unwrap();
        mt.mount("/b", Arc::new(MemoryFilesystem::new())).unwrap();
        let mut rows = [MountSnapshot::EMPTY; 2];
        let n = mt.copy_mounts_into(&mut rows);
        assert_eq!(n, 2);
        // Longest-first ordering means "/" is beyond the truncation point.
        assert!(rows.iter().all(|r| r.path != "/"));
    }
}

#[cfg(test)]
mod memfs_tests {
    use crate::{MemoryFilesystem, Filesystem, FsError};

    #[test]
    fn write_and_read_file() {
        let fs = MemoryFilesystem::new();
        fs.create_dir("/docs").unwrap();
        fs.write_file("/docs/hello.txt", b"hello world").unwrap();
        let data = fs.read_file("/docs/hello.txt").unwrap();
        assert_eq!(data, b"hello world");
    }

    #[test]
    fn read_nonexistent() {
        let fs = MemoryFilesystem::new();
        assert!(fs.read_file("/nope").is_err());
    }

    #[test]
    fn create_and_list_dir() {
        let fs = MemoryFilesystem::new();
        fs.create_dir("/sub").unwrap();
        fs.write_file("/sub/a.txt", b"a").unwrap();
        fs.write_file("/sub/b.txt", b"b").unwrap();
        let entries = fs.read_dir("/sub").unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn remove_file() {
        let fs = MemoryFilesystem::new();
        fs.create_dir("/d").unwrap();
        fs.write_file("/d/f", b"data").unwrap();
        assert!(fs.exists("/d/f"));
        fs.remove_file("/d/f").unwrap();
        assert!(!fs.exists("/d/f"));
    }

    #[test]
    fn remove_nonempty_dir_fails() {
        let fs = MemoryFilesystem::new();
        fs.create_dir("/d").unwrap();
        fs.write_file("/d/f", b"x").unwrap();
        assert!(fs.remove_dir("/d").is_err());
    }

    #[test]
    fn read_at() {
        let fs = MemoryFilesystem::new();
        fs.create_dir("/d").unwrap();
        fs.write_file("/d/f", b"hello world").unwrap();
        let mut buf = [0u8; 5];
        let n = fs.read_at("/d/f", 6, &mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf, b"world");
    }

    #[test]
    fn write_at() {
        let fs = MemoryFilesystem::new();
        fs.create_dir("/d").unwrap();
        fs.write_file("/d/f", b"hello world").unwrap();
        fs.write_at("/d/f", 6, b"WORLD").unwrap();
        let data = fs.read_file("/d/f").unwrap();
        assert_eq!(data, b"hello WORLD");
    }


    #[test]
    fn metadata() {
        let fs = MemoryFilesystem::new();
        fs.create_dir("/d").unwrap();
        fs.write_file("/d/f", b"abc").unwrap();

        let m = fs.metadata("/d/f").unwrap();
        assert!(!m.is_dir);
        assert_eq!(m.size, 3);

        let m = fs.metadata("/d").unwrap();
        assert!(m.is_dir);
    }

    #[test]
    fn max_size_enforcement() {
        let fs = MemoryFilesystem::with_max_size(10);
        fs.create_dir("/d").unwrap();
        fs.write_file("/d/small", b"hi").unwrap();
        let r = fs.write_file("/d/big", b"this is too long!");
        assert!(r.is_err());
    }

    #[test]
    fn rename_file() {
        let fs = MemoryFilesystem::new();
        fs.create_dir("/d").unwrap();
        fs.write_file("/d/old", b"data").unwrap();
        fs.rename("/d/old", "/d/new").unwrap();
        assert!(!fs.exists("/d/old"));
        assert_eq!(fs.read_file("/d/new").unwrap(), b"data");
    }

    #[test]
    fn rename_to_existing_fails() {
        let fs = MemoryFilesystem::new();
        fs.create_dir("/d").unwrap();
        fs.write_file("/d/src", b"source").unwrap();
        fs.write_file("/d/dst", b"destination").unwrap();
        let r = fs.rename("/d/src", "/d/dst");
        assert!(matches!(r, Err(FsError::AlreadyExists)));
        assert_eq!(fs.read_file("/d/src").unwrap(), b"source");
        assert_eq!(fs.read_file("/d/dst").unwrap(), b"destination");
    }

    #[test]
    fn rename_nonexistent_fails() {
        let fs = MemoryFilesystem::new();
        fs.create_dir("/d").unwrap();
        let r = fs.rename("/d/nope", "/d/dst");
        assert!(matches!(r, Err(FsError::NotFound)));
    }

    #[test]
    fn rename_across_directories() {
        let fs = MemoryFilesystem::new();
        fs.create_dir("/a").unwrap();
        fs.create_dir("/b").unwrap();
        fs.write_file("/a/file", b"moved").unwrap();
        fs.rename("/a/file", "/b/file").unwrap();
        assert!(!fs.exists("/a/file"));
        assert_eq!(fs.read_file("/b/file").unwrap(), b"moved");
    }

    #[test]
    fn rename_preserves_content_on_failure() {
        let fs = MemoryFilesystem::new();
        fs.create_dir("/d").unwrap();
        fs.write_file("/d/src", b"important").unwrap();
        let _ = fs.rename("/d/src", "/nonexistent_dir/dst");
        assert!(fs.exists("/d/src"));
        assert_eq!(fs.read_file("/d/src").unwrap(), b"important");
    }

    #[test]
    fn stats() {
        let fs = MemoryFilesystem::with_max_size(4096 * 100);
        let s = fs.stats().unwrap();
        assert_eq!(s.block_size, 4096);
        assert_eq!(s.total_blocks, 100);
    }

    /// Simulates O_APPEND: write initial data, then append using write_at at file size.
    /// This is exactly what Go's `pack r` does with _pkg_.a archives.
    #[test]
    fn write_at_file_size_simulates_o_append() {
        let fs = MemoryFilesystem::new();
        fs.create_dir("/tmp").unwrap();

        let header = b"!<arch>\n";
        let member1 = b"__.PKGDEF original compile data";
        let mut initial = Vec::new();
        initial.extend_from_slice(header);
        initial.extend_from_slice(member1);
        fs.write_file("/tmp/pkg.a", &initial).unwrap();

        let size_before = fs.metadata("/tmp/pkg.a").unwrap().size as usize;
        assert_eq!(size_before, header.len() + member1.len());

        let new_member = b"cpu.o appended by pack r";
        fs.write_at("/tmp/pkg.a", size_before, new_member).unwrap();

        let result = fs.read_file("/tmp/pkg.a").unwrap();
        assert_eq!(&result[..8], b"!<arch>\n", "archive header must be preserved");
        assert_eq!(result.len(), initial.len() + new_member.len());
        assert_eq!(&result[initial.len()..], new_member);
    }

    /// Writing at offset 0 on an existing file must overwrite, not append.
    #[test]
    fn write_at_zero_overwrites() {
        let fs = MemoryFilesystem::new();
        fs.create_dir("/tmp").unwrap();
        fs.write_file("/tmp/f", b"AAAAAAAAAA").unwrap();
        fs.write_at("/tmp/f", 0, b"BB").unwrap();
        let data = fs.read_file("/tmp/f").unwrap();
        assert_eq!(&data, b"BBAAAAAAAA");
    }
}

/// Tests for the merge of `MountTable` and `akuma_isolation::MountNamespace` into
/// one `MountSet<const MAX>` (`TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §4).
///
/// The two types differed only in capacity (8 vs 16) and in whether they called
/// the trailing-slash helper or inlined it — so the things worth pinning are the
/// per-instantiation capacity (the one thing a generic can silently get wrong)
/// and the normalisation the inlined copies used to do by hand.
#[cfg(test)]
mod mount_set_tests {
    extern crate alloc;
    use alloc::format;
    use alloc::sync::Arc;
    use crate::{Filesystem, FsError, MemoryFilesystem, MountSet};

    fn memfs() -> Arc<dyn Filesystem> {
        Arc::new(MemoryFilesystem::new())
    }

    /// `MAX` is per-instantiation, not shared. The kernel's table holds 8 and a
    /// container namespace holds 16; a generic that leaked one bound into the
    /// other would be invisible until a box hit the 9th mount.
    #[test]
    fn capacity_is_per_instantiation() {
        let mut small: MountSet<8> = MountSet::new();
        for i in 0..8 {
            small.mount(&format!("/m{i}"), memfs()).unwrap();
        }
        assert_eq!(small.len(), 8);
        assert_eq!(small.mount("/m8", memfs()).unwrap_err(), FsError::NoSpace);

        let mut big: MountSet<16> = MountSet::new();
        for i in 0..16 {
            big.mount(&format!("/m{i}"), memfs()).unwrap();
        }
        assert_eq!(big.len(), 16);
        assert_eq!(big.mount("/m16", memfs()).unwrap_err(), FsError::NoSpace);
    }

    #[test]
    fn new_set_is_empty() {
        let set: MountSet<8> = MountSet::new();
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
        assert!(set.resolve("/anything").is_none());
    }

    /// Trailing slashes must not change which mount is chosen, nor the relative
    /// path handed to it. The namespace copy open-coded this three times.
    #[test]
    fn trailing_slashes_are_normalised_away() {
        let mut set: MountSet<8> = MountSet::new();
        set.mount("/", memfs()).unwrap();
        set.mount("/data", memfs()).unwrap();

        // Exactly the mount point, with and without the slash.
        assert_eq!(set.resolve("/data").unwrap().1, "/");
        assert_eq!(set.resolve("/data/").unwrap().1, "/");
        // Below it.
        assert_eq!(set.resolve("/data/x/").unwrap().1, "/x");
        // The empty path collapses to root rather than matching nothing.
        assert_eq!(set.resolve("").unwrap().1, "/");
        assert_eq!(set.resolve("/").unwrap().1, "/");
    }

    /// Longest mount path wins regardless of insertion order — `mount` keeps the
    /// vec sorted by descending path length, and `resolve` relies on that.
    #[test]
    fn deepest_mount_wins_regardless_of_order() {
        let mut set: MountSet<8> = MountSet::new();
        set.mount("/a/b", memfs()).unwrap();
        set.mount("/", memfs()).unwrap();
        set.mount("/a", memfs()).unwrap();

        assert_eq!(set.resolve("/a/b/c").unwrap().1, "/c");
        assert_eq!(set.resolve("/a/z").unwrap().1, "/z");
        // `/ab` must NOT match the `/a` mount — a prefix match has to land on a
        // path separator.
        assert_eq!(set.resolve("/ab").unwrap().1, "/ab");
    }

    /// `resolve` and `resolve_arc` must agree; the latter exists only so the
    /// caller can drop its lock before doing I/O.
    #[test]
    fn resolve_and_resolve_arc_agree() {
        let mut set: MountSet<8> = MountSet::new();
        set.mount("/", memfs()).unwrap();
        set.mount("/proc", memfs()).unwrap();

        for p in ["/", "/proc", "/proc/", "/proc/1/status", "/etc/passwd", ""] {
            let borrowed = set.resolve(p).map(|(_, rel)| alloc::string::String::from(rel));
            let owned = set.resolve_arc(p).map(|(_, rel)| rel);
            assert_eq!(borrowed, owned, "disagreement on {p:?}");
        }
    }

    #[test]
    fn mount_rejects_duplicates_and_unmount_reports_missing() {
        let mut set: MountSet<8> = MountSet::new();
        set.mount("/x", memfs()).unwrap();
        assert_eq!(set.mount("/x", memfs()).unwrap_err(), FsError::AlreadyExists);
        assert_eq!(set.unmount("/nope").unwrap_err(), FsError::NotFound);
        set.unmount("/x").unwrap();
        assert!(set.is_empty());
    }

    #[test]
    fn child_mount_points_lists_direct_children_only() {
        let mut set: MountSet<16> = MountSet::new();
        set.mount("/", memfs()).unwrap();
        set.mount("/a", memfs()).unwrap();
        set.mount("/a/b", memfs()).unwrap();
        set.mount("/a/b/c", memfs()).unwrap();

        let mut root: alloc::vec::Vec<_> =
            set.child_mount_points("/").into_iter().map(|e| e.name).collect();
        root.sort();
        assert_eq!(root, ["a"], "only depth-1 children of /");

        let a: alloc::vec::Vec<_> =
            set.child_mount_points("/a").into_iter().map(|e| e.name).collect();
        assert_eq!(a, ["b"]);

        // Trailing slash must not change the answer.
        let a_slash: alloc::vec::Vec<_> =
            set.child_mount_points("/a/").into_iter().map(|e| e.name).collect();
        assert_eq!(a_slash, ["b"]);
    }

    #[test]
    fn get_fs_matches_the_mount_point_exactly() {
        let mut set: MountSet<8> = MountSet::new();
        set.mount("/", memfs()).unwrap();
        set.mount("/data", memfs()).unwrap();

        assert!(set.get_fs("/data").is_some());
        assert!(set.get_fs("/data/").is_some(), "normalised before matching");
        assert!(set.get_fs("/data/sub").is_none(), "not a mount point");
    }

    /// The one-shot root swap, on the shared type. There is deliberately no
    /// unguarded `replace_root`; this is the only write to an existing mount's
    /// `fs` in the tree, and the `expected` check is what makes it safe.
    #[test]
    fn pristine_root_swaps_once_and_never_again() {
        struct Named(&'static str);
        impl Filesystem for Named {
            fn name(&self) -> &str { self.0 }
            fn read_dir(&self, _: &str) -> Result<alloc::vec::Vec<crate::DirEntry>, FsError> { Err(FsError::NotFound) }
            fn read_file(&self, _: &str) -> Result<alloc::vec::Vec<u8>, FsError> { Err(FsError::NotFound) }
            fn write_file(&self, _: &str, _: &[u8]) -> Result<(), FsError> { Err(FsError::NotFound) }
            fn create_dir(&self, _: &str) -> Result<(), FsError> { Err(FsError::NotFound) }
            fn remove_file(&self, _: &str) -> Result<(), FsError> { Err(FsError::NotFound) }
            fn remove_dir(&self, _: &str) -> Result<(), FsError> { Err(FsError::NotFound) }
            fn exists(&self, _: &str) -> bool { false }
            fn metadata(&self, _: &str) -> Result<crate::Metadata, FsError> { Err(FsError::NotFound) }
            fn stats(&self) -> Result<crate::FsStats, FsError> { Err(FsError::NotFound) }
        }

        let mut set: MountSet<16> = MountSet::new();
        set.mount("/", Arc::new(Named("subdirfs"))).unwrap();

        set.replace_pristine_root("subdirfs", Arc::new(Named("overlay"))).unwrap();
        assert_eq!(set.resolve("/x").unwrap().0.name(), "overlay");
        assert_eq!(set.list_mounts().len(), 1, "a swap, not a stack");

        // No longer pristine: nothing can redirect it again.
        assert_eq!(
            set.replace_pristine_root("subdirfs", Arc::new(Named("attacker"))).unwrap_err(),
            FsError::PermissionDenied
        );
        assert_eq!(set.resolve("/x").unwrap().0.name(), "overlay");

        // And a set with no root cannot have one installed this way.
        let mut rootless: MountSet<16> = MountSet::new();
        assert_eq!(
            rootless.replace_pristine_root("subdirfs", Arc::new(Named("x"))).unwrap_err(),
            FsError::PermissionDenied
        );
    }
}
