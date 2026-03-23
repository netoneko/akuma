use alloc::vec;
use alloc::vec::Vec;
use crate::BlockDevice;
use crate::Ext2Filesystem;
use akuma_vfs::Filesystem;

/// In-memory block device backed by a `Vec<u8>`.
struct MemBlockDevice {
    data: spinning_top::Spinlock<Vec<u8>>,
}

impl MemBlockDevice {
    fn new(size: usize) -> Self {
        Self {
            data: spinning_top::Spinlock::new(vec![0u8; size]),
        }
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            data: spinning_top::Spinlock::new(bytes.to_vec()),
        }
    }
}

impl BlockDevice for MemBlockDevice {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        let data = self.data.lock();
        let off = offset as usize;
        if off + buf.len() > data.len() {
            return Err(());
        }
        buf.copy_from_slice(&data[off..off + buf.len()]);
        Ok(())
    }

    fn write_bytes(&self, offset: u64, buf: &[u8]) -> Result<(), ()> {
        let mut data = self.data.lock();
        let off = offset as usize;
        if off + buf.len() > data.len() {
            return Err(());
        }
        data[off..off + buf.len()].copy_from_slice(buf);
        Ok(())
    }
}

/// Load a test fixture image from the tests/fixtures directory.
fn load_fixture(name: &str) -> MemBlockDevice {
    let path = alloc::format!(
        "{}/tests/fixtures/{}",
        env!("CARGO_MANIFEST_DIR"),
        name
    );
    extern crate std;
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("failed to read fixture {path}: {e}"));
    MemBlockDevice::from_bytes(&bytes)
}

fn mount_empty() -> Ext2Filesystem<MemBlockDevice> {
    Ext2Filesystem::new(load_fixture("test.ext2"), || 0).unwrap()
}

fn mount_populated() -> Ext2Filesystem<MemBlockDevice> {
    Ext2Filesystem::new(load_fixture("populated.ext2"), || 0).unwrap()
}

// ── BlockDevice unit tests ──────────────────────────────────────────

#[test]
fn block_device_roundtrip() {
    let dev = MemBlockDevice::new(4096);
    dev.write_bytes(100, b"hello").unwrap();
    let mut buf = [0u8; 5];
    dev.read_bytes(100, &mut buf).unwrap();
    assert_eq!(&buf, b"hello");
}

#[test]
fn block_device_out_of_bounds() {
    let dev = MemBlockDevice::new(64);
    assert!(dev.read_bytes(60, &mut [0u8; 10]).is_err());
    assert!(dev.write_bytes(60, &[0u8; 10]).is_err());
}

// ── Mount / unmount ─────────────────────────────────────────────────

#[test]
fn mount_zeroed_disk_fails() {
    let dev = MemBlockDevice::new(1024 * 1024);
    let result = Ext2Filesystem::new(dev, || 0);
    assert!(result.is_err(), "zeroed disk should not have valid ext2 magic");
}

#[test]
fn mount_bad_magic_fails() {
    let dev = MemBlockDevice::new(1024 * 1024);
    dev.write_bytes(1024, &[0xDE, 0xAD]).unwrap();
    let result = Ext2Filesystem::new(dev, || 0);
    assert!(result.is_err());
}

#[test]
fn mount_valid_empty_image() {
    let fs = mount_empty();
    assert_eq!(fs.name(), "ext2");
}

#[test]
fn mount_valid_populated_image() {
    let fs = mount_populated();
    assert_eq!(fs.name(), "ext2");
}

// ── Directory listing ───────────────────────────────────────────────

#[test]
fn read_root_dir() {
    let fs = mount_empty();
    let entries = fs.read_dir("/").unwrap();
    assert!(
        entries.iter().any(|e| e.name == "lost+found"),
        "root dir should contain lost+found"
    );
}

#[test]
fn read_populated_testdir() {
    let fs = mount_populated();
    let entries = fs.read_dir("/testdir").unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"hello.txt"), "missing hello.txt: {names:?}");
    assert!(names.contains(&"multi.txt"), "missing multi.txt: {names:?}");
    assert!(names.contains(&"subdir"), "missing subdir: {names:?}");
}

// ── File reading ────────────────────────────────────────────────────

#[test]
fn read_file_contents() {
    let fs = mount_populated();
    let data = fs.read_file("/testdir/hello.txt").unwrap();
    assert_eq!(data, b"Hello from ext2 test!\n");
}

#[test]
fn read_file_nonexistent() {
    let fs = mount_populated();
    assert!(fs.read_file("/no/such/file").is_err());
}

#[test]
fn read_at_partial() {
    let fs = mount_populated();
    let mut buf = [0u8; 5];
    let n = fs.read_at("/testdir/hello.txt", 6, &mut buf).unwrap();
    assert_eq!(n, 5);
    assert_eq!(&buf, b"from ");
}

// ── File writing ────────────────────────────────────────────────────

#[test]
fn write_and_read_back() {
    let fs = mount_empty();
    fs.write_file("/newfile.txt", b"test data").unwrap();
    let data = fs.read_file("/newfile.txt").unwrap();
    assert_eq!(data, b"test data");
}

#[test]
fn write_at_offset() {
    let fs = mount_empty();
    fs.write_file("/f.txt", b"hello world").unwrap();
    fs.write_at("/f.txt", 6, b"WORLD").unwrap();
    let data = fs.read_file("/f.txt").unwrap();
    assert_eq!(data, b"hello WORLD");
}

#[test]
fn append_to_file() {
    let fs = mount_empty();
    fs.write_file("/f.txt", b"hello").unwrap();
    fs.append_file("/f.txt", b" world").unwrap();
    let data = fs.read_file("/f.txt").unwrap();
    assert_eq!(data, b"hello world");
}

// ── Directory creation ──────────────────────────────────────────────

#[test]
fn create_dir_is_findable() {
    let fs = mount_empty();
    fs.create_dir("/findme").unwrap();
    assert!(fs.exists("/findme"), "created dir should be findable via lookup");
    let m = fs.metadata("/findme").unwrap();
    assert!(m.is_dir, "created entry should be a directory");
}

#[test]
fn create_dir_and_write_files() {
    let fs = mount_empty();
    fs.create_dir("/sub").unwrap();
    fs.write_file("/sub/a.txt", b"aaa").unwrap();
    fs.write_file("/sub/b.txt", b"bbb").unwrap();
    assert_eq!(fs.read_file("/sub/a.txt").unwrap(), b"aaa");
    assert_eq!(fs.read_file("/sub/b.txt").unwrap(), b"bbb");
}

#[test]
fn create_nested_dirs() {
    let fs = mount_empty();
    fs.create_dir("/a").unwrap();
    fs.create_dir("/a/b").unwrap();
    fs.create_dir("/a/b/c").unwrap();
    fs.write_file("/a/b/c/deep.txt", b"deep").unwrap();
    assert_eq!(fs.read_file("/a/b/c/deep.txt").unwrap(), b"deep");
}

// ── File removal ────────────────────────────────────────────────────

#[test]
fn remove_file_works() {
    let fs = mount_empty();
    fs.write_file("/del.txt", b"bye").unwrap();
    assert!(fs.exists("/del.txt"));
    fs.remove_file("/del.txt").unwrap();
    assert!(!fs.exists("/del.txt"));
}

#[test]
fn remove_dir_works() {
    let fs = mount_empty();
    fs.create_dir("/rmdir").unwrap();
    assert!(fs.exists("/rmdir"));
    fs.remove_dir("/rmdir").unwrap();
    assert!(!fs.exists("/rmdir"));
}

// ── Metadata ────────────────────────────────────────────────────────

#[test]
fn metadata_file() {
    let fs = mount_empty();
    fs.write_file("/meta.txt", b"abc").unwrap();
    let m = fs.metadata("/meta.txt").unwrap();
    assert!(!m.is_dir);
    assert_eq!(m.size, 3);
}

#[test]
fn metadata_dir() {
    let fs = mount_empty();
    fs.create_dir("/metadir").unwrap();
    let m = fs.metadata("/metadir").unwrap();
    assert!(m.is_dir);
}

#[test]
fn metadata_nonexistent() {
    let fs = mount_empty();
    assert!(fs.metadata("/nope").is_err());
}

// ── Rename ──────────────────────────────────────────────────────────

#[test]
fn rename_file() {
    let fs = mount_empty();
    fs.write_file("/old.txt", b"data").unwrap();
    fs.rename("/old.txt", "/new.txt").unwrap();
    assert!(!fs.exists("/old.txt"));
    assert_eq!(fs.read_file("/new.txt").unwrap(), b"data");
}

// ── Exists ──────────────────────────────────────────────────────────

#[test]
fn exists_root() {
    let fs = mount_empty();
    assert!(fs.exists("/"));
}

#[test]
fn exists_lost_and_found() {
    let fs = mount_empty();
    assert!(fs.exists("/lost+found"));
}

// ── Stats ───────────────────────────────────────────────────────────

#[test]
fn stats_reports_block_size() {
    let fs = mount_empty();
    let s = fs.stats().unwrap();
    assert!(s.block_size > 0);
    assert!(s.total_blocks > 0);
    assert!(s.free_blocks <= s.total_blocks);
}

// ── Symlinks ────────────────────────────────────────────────────────

#[test]
fn create_and_read_symlink() {
    let fs = mount_empty();
    fs.write_file("/target.txt", b"hello").unwrap();
    fs.create_symlink("/link.txt", "target.txt").unwrap();
    assert!(fs.exists("/link.txt"));
}

#[test]
fn populated_image_has_symlink() {
    let fs = mount_populated();
    assert!(fs.exists("/testdir/link.txt"));
}

// ── Directory removal edge cases ────────────────────────────────────

#[test]
fn remove_nonempty_dir_fails() {
    let fs = mount_empty();
    fs.create_dir("/parent").unwrap();
    fs.write_file("/parent/child.txt", b"x").unwrap();
    let err = fs.remove_dir("/parent").unwrap_err();
    assert_eq!(err, akuma_vfs::FsError::DirectoryNotEmpty);
}

#[test]
fn remove_dir_with_subdirs_fails() {
    let fs = mount_empty();
    fs.create_dir("/parent").unwrap();
    fs.create_dir("/parent/child").unwrap();
    let err = fs.remove_dir("/parent").unwrap_err();
    assert_eq!(err, akuma_vfs::FsError::DirectoryNotEmpty);
}

#[test]
fn remove_dir_after_clearing_children() {
    let fs = mount_empty();
    fs.create_dir("/d").unwrap();
    fs.write_file("/d/a.txt", b"a").unwrap();
    fs.write_file("/d/b.txt", b"b").unwrap();
    fs.write_file("/d/c.txt", b"c").unwrap();

    assert_eq!(
        fs.remove_dir("/d").unwrap_err(),
        akuma_vfs::FsError::DirectoryNotEmpty
    );

    fs.remove_file("/d/a.txt").unwrap();
    fs.remove_file("/d/b.txt").unwrap();
    fs.remove_file("/d/c.txt").unwrap();

    fs.remove_dir("/d").unwrap();
    assert!(!fs.exists("/d"));
}

#[test]
fn remove_many_entries_then_rmdir() {
    let fs = mount_empty();
    fs.create_dir("/big").unwrap();

    let count = 64;
    for i in 0..count {
        let name = alloc::format!("/big/{:02x}", i);
        fs.create_dir(&name).unwrap();
    }

    let entries = fs.read_dir("/big").unwrap();
    assert_eq!(entries.len(), count);

    for i in 0..count {
        let name = alloc::format!("/big/{:02x}", i);
        fs.remove_dir(&name).unwrap();
    }

    let entries = fs.read_dir("/big").unwrap();
    assert_eq!(entries.len(), 0, "all children should be gone: {entries:?}");
    fs.remove_dir("/big").unwrap();
    assert!(!fs.exists("/big"));
}

#[test]
fn remove_entries_in_reverse_order() {
    let fs = mount_empty();
    fs.create_dir("/rev").unwrap();

    let count = 32;
    for i in 0..count {
        let name = alloc::format!("/rev/item_{:02}", i);
        fs.write_file(&name, b"data").unwrap();
    }

    for i in (0..count).rev() {
        let name = alloc::format!("/rev/item_{:02}", i);
        fs.remove_file(&name).unwrap();
    }

    let entries = fs.read_dir("/rev").unwrap();
    assert_eq!(entries.len(), 0);
    fs.remove_dir("/rev").unwrap();
}

#[test]
fn remove_interleaved_files_and_dirs() {
    let fs = mount_empty();
    fs.create_dir("/mix").unwrap();

    for i in 0..16u32 {
        let dname = alloc::format!("/mix/d{:02}", i);
        let fname = alloc::format!("/mix/f{:02}.txt", i);
        fs.create_dir(&dname).unwrap();
        fs.write_file(&fname, b"x").unwrap();
    }

    let entries = fs.read_dir("/mix").unwrap();
    assert_eq!(entries.len(), 32);

    for i in 0..16u32 {
        let dname = alloc::format!("/mix/d{:02}", i);
        let fname = alloc::format!("/mix/f{:02}.txt", i);
        fs.remove_file(&fname).unwrap();
        fs.remove_dir(&dname).unwrap();
    }

    let entries = fs.read_dir("/mix").unwrap();
    assert_eq!(entries.len(), 0);
    fs.remove_dir("/mix").unwrap();
}

#[test]
fn remove_first_entry_in_directory() {
    let fs = mount_empty();
    fs.create_dir("/first").unwrap();
    fs.write_file("/first/aaa", b"a").unwrap();
    fs.write_file("/first/bbb", b"b").unwrap();
    fs.write_file("/first/ccc", b"c").unwrap();

    fs.remove_file("/first/aaa").unwrap();

    let entries = fs.read_dir("/first").unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(!names.contains(&"aaa"));
    assert!(names.contains(&"bbb"));
    assert!(names.contains(&"ccc"));
}

#[test]
fn remove_middle_entry_in_directory() {
    let fs = mount_empty();
    fs.create_dir("/mid").unwrap();
    fs.write_file("/mid/aaa", b"a").unwrap();
    fs.write_file("/mid/bbb", b"b").unwrap();
    fs.write_file("/mid/ccc", b"c").unwrap();

    fs.remove_file("/mid/bbb").unwrap();

    let entries = fs.read_dir("/mid").unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"aaa"));
    assert!(!names.contains(&"bbb"));
    assert!(names.contains(&"ccc"));
}

#[test]
fn remove_last_entry_in_directory() {
    let fs = mount_empty();
    fs.create_dir("/last").unwrap();
    fs.write_file("/last/aaa", b"a").unwrap();
    fs.write_file("/last/bbb", b"b").unwrap();
    fs.write_file("/last/ccc", b"c").unwrap();

    fs.remove_file("/last/ccc").unwrap();

    let entries = fs.read_dir("/last").unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"aaa"));
    assert!(names.contains(&"bbb"));
    assert!(!names.contains(&"ccc"));
}

#[test]
fn reuse_space_after_removal() {
    let fs = mount_empty();
    fs.create_dir("/reuse").unwrap();
    fs.write_file("/reuse/old", b"old").unwrap();
    fs.remove_file("/reuse/old").unwrap();
    fs.write_file("/reuse/new", b"new").unwrap();

    let entries = fs.read_dir("/reuse").unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "new");
    assert_eq!(fs.read_file("/reuse/new").unwrap(), b"new");
}

#[test]
fn remove_file_from_dir_does_not_affect_file() {
    let fs = mount_empty();
    fs.write_file("/not_a_dir.txt", b"content").unwrap();
    let err = fs.remove_dir("/not_a_dir.txt").unwrap_err();
    assert_eq!(err, akuma_vfs::FsError::NotADirectory);
}

#[test]
fn remove_dir_on_file_fails() {
    let fs = mount_empty();
    fs.write_file("/regular.txt", b"x").unwrap();
    let err = fs.remove_dir("/regular.txt").unwrap_err();
    assert_eq!(err, akuma_vfs::FsError::NotADirectory);
}

#[test]
fn remove_file_on_dir_fails() {
    let fs = mount_empty();
    fs.create_dir("/adir").unwrap();
    let err = fs.remove_file("/adir").unwrap_err();
    assert_eq!(err, akuma_vfs::FsError::NotAFile);
}
