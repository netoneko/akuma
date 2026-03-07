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
    use crate::{MountTable, MemoryFilesystem, Filesystem};

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
}

#[cfg(test)]
mod memfs_tests {
    use crate::{MemoryFilesystem, Filesystem};

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
    fn append_file() {
        let fs = MemoryFilesystem::new();
        fs.create_dir("/d").unwrap();
        fs.write_file("/d/f", b"hello").unwrap();
        fs.append_file("/d/f", b" world").unwrap();
        let data = fs.read_file("/d/f").unwrap();
        assert_eq!(data, b"hello world");
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
    fn stats() {
        let fs = MemoryFilesystem::with_max_size(4096 * 100);
        let s = fs.stats().unwrap();
        assert_eq!(s.block_size, 4096);
        assert_eq!(s.total_blocks, 100);
    }
}
