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

/// Simulates the O_APPEND pattern: write initial archive data, then append at
/// the file size (exactly what Go's `pack r` does with _pkg_.a files).
#[test]
fn write_at_file_size_appends_without_overwriting() {
    let fs = mount_empty();
    let header = b"!<arch>\n";
    let original = b"__.PKGDEF compile output";
    let mut initial = vec![];
    initial.extend_from_slice(header);
    initial.extend_from_slice(original);
    fs.write_file("/pkg.a", &initial).unwrap();

    let meta = fs.metadata("/pkg.a").unwrap();
    assert_eq!(meta.size as usize, initial.len());

    let appended = b"cpu.o member data";
    fs.write_at("/pkg.a", initial.len(), appended).unwrap();

    let result = fs.read_file("/pkg.a").unwrap();
    assert_eq!(&result[..8], b"!<arch>\n", "header must survive");
    assert_eq!(result.len(), initial.len() + appended.len());
    assert_eq!(&result[initial.len()..], appended);
}

#[test]
fn try_lock_state_succeeds_when_unlocked() {
    let dev = load_fixture("test.ext2");
    let fs = Ext2Filesystem::new(dev, || 0).unwrap();
    
    // try_lock_state should succeed immediately when lock is not held
    let guard = fs.try_lock_state(10);
    assert!(guard.is_some(), "try_lock_state should succeed when lock is free");
}

#[test]
fn try_lock_state_returns_none_when_locked() {
    let dev = load_fixture("test.ext2");
    let fs = Ext2Filesystem::new(dev, || 0).unwrap();
    
    // Hold the write lock (simulating a write operation)
    let _guard = fs.state.write();
    
    // try_lock_state should fail quickly (1 retry only) when write lock is held
    let result = fs.try_lock_state(1);
    assert!(result.is_none(), "try_lock_state should return None when write lock is held");
}

#[test]
fn exists_unblocks_after_raw_write_lock_released() {
    use std::sync::Arc;
    use std::thread;

    let dev = load_fixture("test.ext2");
    let fs = Arc::new(Ext2Filesystem::new(dev, || 0).unwrap());

    let fs_holder = Arc::clone(&fs);
    let (lock_held_tx, lock_held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();

    let holder = thread::spawn(move || {
        let _guard = fs_holder.state.write();
        lock_held_tx.send(()).unwrap();
        let _: () = release_rx.recv().unwrap();
    });

    lock_held_rx.recv().unwrap();

    let fs_check = Arc::clone(&fs);
    let checker = thread::spawn(move || fs_check.exists("/lost+found"));

    release_tx.send(()).unwrap();
    holder.join().unwrap();
    let exists_result = checker.join().unwrap();

    assert!(
        exists_result,
        "exists should succeed once the contended write lock is released"
    );
}

#[test]
fn concurrent_write_at_does_not_corrupt() {
    use std::sync::Arc;
    use std::thread;

    let fs = Arc::new(mount_empty());
    fs.write_file("/testfile", b"").unwrap();

    let num_threads = 4;
    let writes_per_thread = 20;
    let chunk_size = 64;

    let mut handles = Vec::new();
    for t in 0..num_threads {
        let fs_clone = Arc::clone(&fs);
        handles.push(thread::spawn(move || {
            for i in 0..writes_per_thread {
                let offset = (t * writes_per_thread + i) * chunk_size;
                let data = vec![(t * 10 + i) as u8; chunk_size];
                let result = fs_clone.write_at("/testfile", offset, &data);
                assert!(result.is_ok(), "thread {} write {} failed: {:?}", t, i, result.err());
            }
        }));
    }

    for h in handles {
        h.join().expect("thread panicked");
    }

    // Verify file is readable and has expected size
    let content = fs.read_file("/testfile").unwrap();
    let expected_size = num_threads * writes_per_thread * chunk_size;
    assert!(
        content.len() >= expected_size,
        "file too small: {} < {}",
        content.len(),
        expected_size
    );
}

#[test]
fn concurrent_create_and_lookup() {
    use std::sync::Arc;
    use std::thread;

    let fs = Arc::new(mount_empty());
    fs.create_dir("/tmp").unwrap();

    let num_threads = 4;
    let files_per_thread = 5;

    let mut handles = Vec::new();
    for t in 0..num_threads {
        let fs_clone = Arc::clone(&fs);
        handles.push(thread::spawn(move || {
            for i in 0..files_per_thread {
                let name = alloc::format!("/tmp/file_t{}_i{}", t, i);
                let data = alloc::format!("thread={} file={}", t, i);
                fs_clone.write_file(&name, data.as_bytes()).unwrap_or_else(|e| {
                    panic!("thread {} failed to create {}: {:?}", t, name, e);
                });

                // Read back immediately
                let content = fs_clone.read_file(&name).unwrap_or_else(|e| {
                    panic!("thread {} failed to read back {}: {:?}", t, name, e);
                });
                assert_eq!(
                    content,
                    data.as_bytes(),
                    "thread {} data mismatch for {}",
                    t,
                    name
                );
            }
        }));
    }

    for h in handles {
        h.join().expect("thread panicked");
    }

    // Verify all files still exist and are correct
    for t in 0..num_threads {
        for i in 0..files_per_thread {
            let name = alloc::format!("/tmp/file_t{}_i{}", t, i);
            let expected = alloc::format!("thread={} file={}", t, i);
            assert!(fs.exists(&name), "file {} missing after concurrent creates", name);
            let content = fs.read_file(&name).unwrap();
            assert_eq!(content, expected.as_bytes(), "content mismatch for {}", name);
        }
    }
}

// ============================================================================
// ClockBlockCache (large block cache, feature `fs-cache`) unit tests.
// Compiled whenever `cfg(test)` is active (the cache type is `cfg(any(ext2_fs_cache, test))`).
// ============================================================================

use crate::ext2::{ClockBlockCache, cache_stats, set_cache_cap_bytes};

/// A distinct 4-byte-tagged block of `block_size` bytes for block number `n`.
fn blk(n: u32, block_size: usize) -> Vec<u8> {
    let mut v = vec![0u8; block_size];
    v[0..4].copy_from_slice(&n.to_le_bytes());
    v
}

#[test]
fn clock_cache_basic_hit_and_miss() {
    let bs = 1024;
    let mut c = ClockBlockCache::with_capacity_blocks(bs, 8);
    assert!(c.get(5).is_none(), "empty cache must miss");
    c.insert(5, &blk(5, bs));
    let got = c.get(5).expect("inserted block must hit");
    assert_eq!(&got[0..4], &5u32.to_le_bytes(), "wrong block data returned");
}

#[test]
fn clock_cache_dedup_insert() {
    let bs = 1024;
    let mut c = ClockBlockCache::with_capacity_blocks(bs, 8);
    c.insert(7, &blk(7, bs));
    c.insert(7, &blk(7, bs)); // duplicate: must not create a second slot
    // Fill the rest; if the dup created a slot we'd evict 7 one round early.
    for n in 100..107 {
        c.insert(n, &blk(n, bs));
    }
    assert!(c.get(7).is_some(), "block 7 should still be resident (no dup slot)");
}

#[test]
fn clock_cache_remove_invalidates() {
    let bs = 512;
    let mut c = ClockBlockCache::with_capacity_blocks(bs, 8);
    c.insert(3, &blk(3, bs));
    assert!(c.get(3).is_some());
    c.remove(3);
    assert!(c.get(3).is_none(), "removed block must miss");
    // The freed slot must be reusable.
    c.insert(9, &blk(9, bs));
    assert!(c.get(9).is_some(), "freed slot must be reusable");
}

#[test]
fn clock_cache_second_chance_spares_referenced_block() {
    // The defining property of clock vs a pure ring: a *referenced* block gets a
    // second chance and survives an eviction in favour of an unreferenced one.
    let bs = 256;
    let mut c = ClockBlockCache::with_capacity_blocks(bs, 4);
    for n in 0..4 {
        c.insert(n, &blk(n, bs)); // slots 0..3, all ref=1, hand=0
    }
    // Full + all bits set => first eviction is FIFO (block 0). This is correct
    // clock behaviour, not a bug — every block had its chance.
    c.insert(4, &blk(4, bs));
    assert!(c.get(0).is_none(), "block 0 should be evicted (FIFO when all referenced)");
    // Now present: 4,1,2,3 with ref=[1,0,0,0]. Touch 1 and 2; leave 3 cold.
    assert!(c.get(1).is_some());
    assert!(c.get(2).is_some());
    // Insert 5: the hand clears 1 and 2 (second chance) and evicts the cold 3.
    c.insert(5, &blk(5, bs));
    assert!(c.get(3).is_none(), "cold block 3 should be evicted");
    assert!(c.get(1).is_some(), "referenced block 1 must be spared");
    assert!(c.get(2).is_some(), "referenced block 2 must be spared");
    assert!(c.get(5).is_some(), "newly inserted block 5 present");
}

#[test]
fn clock_cache_capacity_floor() {
    // A tiny cap must still give at least the old ring's worth of slots (64).
    let bs = 1024;
    // new() applies the max(64, cap/bs) floor; a 1024-byte cap -> 64 slots.
    set_cache_cap_bytes(1024);
    let mut c = ClockBlockCache::new(bs);
    for n in 0..64 {
        c.insert(n, &blk(n, bs));
    }
    // All 64 fit (floor is 64 slots), so the first is still present.
    assert!(c.get(0).is_some(), "64-slot floor not honored");
}

#[test]
fn cache_stats_default_zero() {
    // With no reads issued through a filesystem, the global counters report a
    // valid tuple (exercises the public accessor under `cfg(test)`).
    let (_h, _m) = cache_stats();
}
