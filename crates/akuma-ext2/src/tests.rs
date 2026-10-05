use alloc::vec;
use alloc::vec::Vec;
use crate::BlockDevice;
use crate::Ext2Filesystem;
use akuma_vfs::Filesystem;

/// In-memory block device backed by a `Vec<u8>`.
pub struct MemBlockDevice {
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

/// 4 MB / 2048-inode image. `test.ext2` has only **256 inodes**, which is
/// exactly the deferral bound this fixture exists to exceed — a test of "more
/// concurrent pinned unlinks than the list holds" cannot be written against a
/// filesystem that cannot hold that many files. Built with
/// `mke2fs -t ext2 -b 1024 -N 2048 -I 128 -O ^resize_inode,^dir_index`.
pub fn mount_many_inodes() -> Ext2Filesystem<MemBlockDevice> {
    Ext2Filesystem::new(load_fixture("manyinodes.ext2"), || 0).unwrap()
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

/// A whole-index-sized write, read back byte for byte — the size class of an
/// Alpine APKINDEX (~130 KB), which is what apk writes into its cache before
/// re-opening it for signature verification. 1 KiB-block fixtures reach the
/// indirect-block chains at 12 KB, double-indirect at ~4 MB is not reached
/// here but every single- and first-level-indirect block below 128 KB is.
/// A silent mid-file corruption here surfaces downstream as apk reporting
/// `UNTRUSTED signature` over a fetch that was delivered intact (amd64,
/// 2026-09-04).
#[test]
fn write_read_round_trip_index_sized() {
    let fs = mount_empty();
    // Deterministic pseudo-random fill: every byte position differs, so a
    // wrong block mapping cannot pass by repeating a pattern.
    let data: alloc::vec::Vec<u8> = (0..131_072u32)
        .map(|i| ((i ^ 0xA5A5_A5A5).wrapping_mul(26_544_357) >> 11) as u8)
        .collect();
    fs.write_file("/big.bin", &data).unwrap();
    let back = fs.read_file("/big.bin").unwrap();
    assert_eq!(back.len(), data.len(), "length must survive the round trip");
    assert_eq!(back, data, "every byte must survive the round trip");
}

#[test]
fn write_at_offset() {
    let fs = mount_empty();
    fs.write_file("/f.txt", b"hello world").unwrap();
    fs.write_at("/f.txt", 6, b"WORLD").unwrap();
    let data = fs.read_file("/f.txt").unwrap();
    assert_eq!(data, b"hello WORLD");
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

/// `rmdir` must leave the freed inode's on-disk record claiming **zero** links.
///
/// A directory lives with `hard_links = 2` (`.` plus the parent's entry).
/// `remove_dir` set `deletion_time` and returned the number to the allocator
/// but never zeroed the count, so every `rmdir` left a record that `e2fsck`
/// reads as corruption: *"inode N is in use, but has dtime set"*, which then
/// cascades into "zero-length directory" and "unconnected directory". Found
/// 2026-08-31 by `e2fsck`-ing `ext2probe-host`'s post-`sync()` image (15
/// leaked inodes in one run, and identically so on the pre-codec baseline —
/// `docs/archive/AKUMA_EXT2_CLEANUP.md` §6.1).
///
/// Read back through [`Inode::parse`] off the device rather than through the
/// live `Inode` value, because the on-disk record is the only thing `e2fsck`
/// ever sees.
#[test]
fn remove_dir_zeroes_the_directorys_link_count() {
    // A real clock, not `mount_empty`'s `|| 0`: what `e2fsck` rejects is the
    // *pairing* of a set `dtime` with a nonzero `links_count`, so a fixture
    // whose `dtime` is always 0 cannot express the bug.
    let fs = Ext2Filesystem::new(load_fixture("test.ext2"), || 1_700_000_000_000_000).unwrap();
    fs.create_dir("/gone").unwrap();
    let ino = fs.lookup_path("/gone").unwrap();

    {
        let state = fs.state.read();
        let live = fs.read_inode(&state, ino).unwrap();
        assert_eq!(live.hard_links, 2, "a fresh directory has `.` + parent");
    }

    fs.remove_dir("/gone").unwrap();

    let state = fs.state.read();
    let dead = fs.read_inode(&state, ino).unwrap();
    assert_ne!(dead.deletion_time, 0, "rmdir must stamp dtime");
    assert_eq!(
        dead.hard_links, 0,
        "a freed directory's record must not still claim links (e2fsck: \
         \"in use, but has dtime set\")"
    );
}

/// A thread killed mid-write must not wedge the mount — and recovery must not
/// need anyone to *ask* whether that thread is alive.
///
/// This is the §4.2a bug's regression test. The old path recorded the write
/// lock's owner tid and, every 10 000 spins, called `is_thread_terminated(tid)`
/// to decide whether to `force_unlock_write()`. On a busy system that question
/// is unanswerable: thread slots are recycled, so by the time anyone asked, the
/// tid usually belonged to a *live* new occupant, the answer came back "alive",
/// and recovery never fired — the mount stayed wedged for good. The old code
/// could not pass this test at all, because nothing here ever marks a thread
/// dead; the recovery below happens purely because the runtime *reports* the
/// death by calling `abandon_tid`.
///
/// `mem::forget` is the kill: every shipped profile builds `panic = "abort"`,
/// so a thread killed at an arbitrary instruction never runs its guard's
/// `Drop` — exactly this shape.
#[test]
fn a_thread_killed_holding_the_write_lock_does_not_wedge_the_mount() {
    let fs = mount_empty();
    fs.create_dir("/before").unwrap();

    // Take the write lock and die holding it.
    let tid = akuma_primitives::preempt::current_tid();
    core::mem::forget(fs.state.write_as(tid));
    assert!(fs.state.try_write_as(tid + 1).is_none(), "wedged, as expected");

    // The runtime reports the death at the TERMINATED->FREE transition.
    assert!(fs.abandon_tid(tid), "the sweep recovers the orphaned write hold");

    // The mount is fully usable again, and the pre-kill state is intact.
    assert!(fs.exists("/before"));
    fs.create_dir("/after").unwrap();
    assert!(fs.exists("/after"));
    fs.write_file("/after/f", b"ok").unwrap();
    assert_eq!(fs.read_file("/after/f").unwrap(), b"ok");
}

/// Read holds were **unrecoverable** in the old design: it tracked one writer
/// tid and nothing else, so a thread killed holding a read lock blocked every
/// future writer forever with no code path that could ever notice.
#[test]
fn a_thread_killed_holding_read_locks_does_not_wedge_the_mount() {
    let fs = mount_empty();
    let tid = akuma_primitives::preempt::current_tid();
    core::mem::forget(fs.state.read_as(tid));
    core::mem::forget(fs.state.read_as(tid));
    assert!(fs.state.try_write_as(tid + 1).is_none(), "two leaked read holds");

    assert!(fs.abandon_tid(tid), "the sweep drains both");
    fs.create_dir("/writable_again").unwrap();
    assert!(fs.exists("/writable_again"));
}

/// Sweeping a tid that holds nothing must not steal a live holder's lock —
/// the property that makes it safe to call the reaper on every thread death.
#[test]
fn sweeping_an_unrelated_tid_leaves_a_live_hold_alone() {
    let fs = mount_empty();
    let tid = akuma_primitives::preempt::current_tid();
    let held = fs.state.write_as(tid);
    assert!(!fs.abandon_tid(tid + 9), "that tid holds nothing here");
    assert!(fs.state.try_read_as(tid + 1).is_none(), "the live hold survived");
    drop(held);
    assert!(fs.state.try_read_as(tid + 1).is_some());
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

// ── AF_UNIX socket nodes (S_IFSOCK) ─────────────────────────────────

/// A socket node must `stat` as `S_IFSOCK` (0o14xxxx), not as a regular file.
///
/// **The type bits are the entire reason this node exists.** A client connecting
/// to a unix socket `stat`s the path and checks `S_ISSOCK` first; before this,
/// `bind` created an ordinary file, `stat` reported `S_IFREG`, and a conformant
/// client refused to connect to a socket that was working perfectly. Found by
/// `nettest-unix path` against its Linux control arm, which reported
/// `mode=0o100644 S_ISSOCK=false`.
#[test]
fn socket_node_stats_as_ifsock() {
    let fs = mount_empty();
    fs.create_socket_node("/probe.sock").unwrap();
    let md = fs.metadata("/probe.sock").unwrap();
    assert_eq!(
        md.mode & 0xF000,
        0xC000,
        "socket node reports mode 0o{:o}, not S_IFSOCK — a client checking S_ISSOCK will refuse to connect",
        md.mode
    );
    assert!(!md.is_dir);
    assert_eq!(md.size, 0, "a socket node holds no data");
}

/// `bind` on a path that already exists must fail, and the error has to be
/// `AlreadyExists` specifically: AF_UNIX maps it to `EADDRINUSE`, which is what
/// tells a daemon it must `unlink` a stale node before it can restart. Silently
/// reusing the node would let two daemons believe they own the same path.
#[test]
fn socket_node_on_existing_path_is_already_exists() {
    let fs = mount_empty();
    fs.write_file("/taken", b"x").unwrap();
    assert!(matches!(
        fs.create_socket_node("/taken"),
        Err(akuma_vfs::FsError::AlreadyExists)
    ));
    fs.create_socket_node("/s.sock").unwrap();
    assert!(matches!(
        fs.create_socket_node("/s.sock"),
        Err(akuma_vfs::FsError::AlreadyExists)
    ));
}

/// A daemon must be able to unlink its own stale node and rebind.
///
/// `remove_file` rejects directories; a socket node is not one, but it is also
/// not a regular file, so this asserts the type check does not sweep it up. If
/// unlink refused, a crashed daemon could never restart — the node would sit
/// there forever making every `bind` fail.
#[test]
fn socket_node_can_be_unlinked_and_recreated() {
    let fs = mount_empty();
    fs.create_socket_node("/s.sock").unwrap();
    fs.remove_file("/s.sock").unwrap();
    assert!(!fs.exists("/s.sock"));
    fs.create_socket_node("/s.sock").unwrap();
    assert_eq!(fs.metadata("/s.sock").unwrap().mode & 0xF000, 0xC000);
}

/// A socket node lives in a directory like anything else, and creating one must
/// not corrupt the directory it goes into — the `EXT2_FT_SOCK` dirent type byte
/// is new, and an unrecognised type byte is how a directory parse goes wrong.
#[test]
fn socket_node_in_a_subdirectory_leaves_the_directory_readable() {
    let fs = mount_empty();
    fs.create_dir("/run").unwrap();
    fs.write_file("/run/before.txt", b"a").unwrap();
    fs.create_socket_node("/run/app.sock").unwrap();
    fs.write_file("/run/after.txt", b"b").unwrap();

    let names: Vec<_> = fs.read_dir("/run").unwrap().into_iter().map(|e| e.name).collect();
    assert!(names.iter().any(|n| n == "app.sock"), "socket node missing from listing: {names:?}");
    assert!(names.iter().any(|n| n == "before.txt"));
    assert!(names.iter().any(|n| n == "after.txt"), "entries after the socket node were lost");
    // And the neighbours still read back correctly.
    assert_eq!(fs.read_file("/run/before.txt").unwrap(), b"a");
    assert_eq!(fs.read_file("/run/after.txt").unwrap(), b"b");
}

/// The node is not a file and must not be readable as one — it holds nothing,
/// and a caller that manages to `read` it is reading whatever the inode's block
/// pointers happen to contain.
#[test]
fn socket_node_is_not_a_regular_file() {
    let fs = mount_empty();
    fs.create_socket_node("/s.sock").unwrap();
    let md = fs.metadata("/s.sock").unwrap();
    assert_ne!(md.mode & 0xF000, 0x8000, "socket node claims to be S_IFREG");
    // Whatever read_file does with it, it must not hand back data.
    if let Ok(data) = fs.read_file("/s.sock") {
        assert!(data.is_empty(), "read a socket node and got {} bytes", data.len());
    }
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

// ── Unlinked-but-still-mapped inodes ────────────────────────────────
//
// Root cause #2 of the self-host `rustc` ICE: a `LazySource::File` mapping names
// its file by raw inode number, so `remove_file` freeing that inode under a live
// mapping made the mapper read `Ok(0)` (zero page) or, once the number was
// reissued, another file's bytes. `docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` §14.
//
// The pin these tests take is what a mapping holds for its whole lifetime.
//
// They are serialized against each other because the pin table is keyed on the
// inode number **alone**, with no filesystem identity (see
// `akuma_primitives::inode_pin` — aliasing can only defer a free, never permit
// one, so it is safe in the kernel). Every test here mounts the *same* fixture,
// so their inode numbers collide exactly; run in parallel, one test's pin would
// make another's inode look pinned. That is the documented behaviour rather than
// a bug, so the tests take a lock instead of weakening the assertion.
extern crate std;
static PIN_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn pin_test_serial() -> std::sync::MutexGuard<'static, ()> {
    PIN_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

std::thread_local! {
    /// Inodes reported through [`crate::init_inode_freed_hook`] **on this test
    /// thread**, newest last. Thread-local, not a shared `Mutex<Vec>`: the hook
    /// is one global registration, but it always fires synchronously on the
    /// thread that called `remove_file`/`remove_dir`, and `cargo test` runs each
    /// `#[test]` on its own thread — so a concurrent unrelated test freeing an
    /// inode with the same (low, fresh-fs) number can't pollute this test's
    /// recording. Was a `static Mutex<Vec<u32>>`; that flaked ~1/15 on the
    /// `!contains(&deferred)` assertion below.
    static FREED_INODES: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Register the recorder once (`Registered` ignores repeat calls) and clear this
/// thread's buffer.
fn record_freed_inodes() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        crate::init_inode_freed_hook(|inode| {
            FREED_INODES.with(|v| v.borrow_mut().push(inode));
        });
    });
    FREED_INODES.with(|v| v.borrow_mut().clear());
}

fn freed_inodes() -> Vec<u32> {
    FREED_INODES.with(|v| v.borrow().clone())
}

/// Reissuing an inode number must drop anything keyed on it.
///
/// The kernel's `file_page_cache` keys on `(inode, file_offset)`, so a recycled
/// number inherits the previous file's cached pages unless they go at the moment
/// the number is released. Deferring the free made this reachable: an unlinked
/// but still-mapped file goes on publishing pages under its number, where before
/// the truncated inode made those fills come back `Ok(0)` and be withheld. The
/// symptom was `rust-lld: ELF section name out of range` on a freshly built
/// rlib — §15.
#[test]
fn freeing_an_inode_reports_it_for_cache_invalidation() {
    let _serial = pin_test_serial();
    let fs = mount_empty();
    record_freed_inodes();

    // Immediate free: no mapping, so the unlink frees the inode inline.
    fs.write_file("/plainfree", b"data").unwrap();
    let plain = fs.resolve_inode("/plainfree").unwrap();
    fs.remove_file("/plainfree").unwrap();
    assert!(
        freed_inodes().contains(&plain),
        "an immediate free must report inode {plain}: {:?}",
        freed_inodes(),
    );

    // Deferred free: reported when the deferral is drained, not at unlink time —
    // the number is still in use until then, so invalidating early would be both
    // wrong and useless.
    record_freed_inodes();
    fs.write_file("/deferredfree", b"data").unwrap();
    let deferred = fs.resolve_inode("/deferredfree").unwrap();
    let pin = akuma_primitives::InodePin::new(deferred);
    fs.remove_file("/deferredfree").unwrap();
    assert!(
        !freed_inodes().contains(&deferred),
        "a pinned inode is not free yet and must not be reported",
    );

    drop(pin);
    fs.write_file("/triggerdrain", b"x").unwrap();
    assert!(
        freed_inodes().contains(&deferred),
        "draining the deferral must report inode {deferred}: {:?}",
        freed_inodes(),
    );
}

/// The exact defect, at the layer that caused it: the data a mapping is reading
/// must survive the unlink of its last name.
#[test]
fn unlink_of_a_pinned_inode_keeps_its_data_readable() {
    let _serial = pin_test_serial();
    let fs = mount_empty();
    fs.write_file("/mapped.rlib", b"REAL FILE CONTENT").unwrap();
    let inode = fs.resolve_inode("/mapped.rlib").unwrap();

    // A mapper takes its pin, exactly as `LazySource::file` does at mmap time.
    let pin = akuma_primitives::InodePin::new(inode);

    fs.remove_file("/mapped.rlib").unwrap();

    // The name is gone...
    assert!(!fs.exists("/mapped.rlib"), "the dirent must be removed");
    // ...but the mapping's next fill still reads the file, not zeros. Before the
    // fix this returned Ok(0) because `remove_file` had truncated i_size to 0.
    let mut buf = [0u8; 17];
    let n = fs.read_at_by_inode(inode, 0, &mut buf).unwrap();
    assert_eq!(n, 17, "a pinned inode must not read short after unlink");
    assert_eq!(&buf, b"REAL FILE CONTENT");

    drop(pin);
}

/// The garbage-bytes half: the freed number must not be handed to the next file
/// created while a mapping still names it.
#[test]
fn a_pinned_inode_number_is_not_reissued_to_a_new_file() {
    let _serial = pin_test_serial();
    let fs = mount_empty();
    fs.write_file("/victim", b"victim data").unwrap();
    let inode = fs.resolve_inode("/victim").unwrap();
    let pin = akuma_primitives::InodePin::new(inode);

    fs.remove_file("/victim").unwrap();

    // Create files until one would plausibly land on the recycled number.
    for i in 0..16 {
        let name = alloc::format!("/filler{i}");
        fs.write_file(&name, b"other file entirely").unwrap();
        assert_ne!(
            fs.resolve_inode(&name).unwrap(),
            inode,
            "inode {inode} was reissued while a mapping still held it",
        );
    }

    // And the mapping still sees its own bytes, not the new file's.
    let mut buf = [0u8; 11];
    assert_eq!(fs.read_at_by_inode(inode, 0, &mut buf).unwrap(), 11);
    assert_eq!(&buf, b"victim data");

    drop(pin);
}

/// Deferral is not a leak: once the last mapping goes, the inode is reclaimed.
#[test]
fn dropping_the_last_pin_lets_the_inode_be_reclaimed() {
    let _serial = pin_test_serial();
    let fs = mount_empty();
    fs.write_file("/transient", b"data").unwrap();
    let inode = fs.resolve_inode("/transient").unwrap();

    let pin = akuma_primitives::InodePin::new(inode);
    fs.remove_file("/transient").unwrap();
    assert_eq!(fs.deferred_free_len(), 1, "the free must be queued");

    // The mapping goes away, and the next allocation drains the queue.
    drop(pin);
    fs.write_file("/after", b"x").unwrap();

    assert_eq!(
        fs.deferred_free_len(),
        0,
        "an unpinned inode must not stay deferred",
    );
    // Reclaimed for real — the number is issued again, which is precisely what
    // must *not* happen while a mapping still holds it (the test above).
    assert_eq!(
        fs.resolve_inode("/after").unwrap(),
        inode,
        "an unpinned inode should return to the allocator",
    );
}

/// The unchanged path — an unlink with no mapping still frees immediately, so
/// the fix costs nothing on the overwhelmingly common case.
#[test]
fn unlink_of_an_unpinned_inode_frees_immediately() {
    let _serial = pin_test_serial();
    let fs = mount_empty();
    fs.write_file("/plain", b"nothing maps me").unwrap();
    let inode = fs.resolve_inode("/plain").unwrap();

    fs.remove_file("/plain").unwrap();

    assert_eq!(fs.deferred_free_len(), 0, "nothing to defer");
    let mut buf = [0u8; 8];
    assert_eq!(
        fs.read_at_by_inode(inode, 0, &mut buf).unwrap(),
        0,
        "an unpinned inode is truncated and freed as before",
    );
}

/// Nothing is leaked by the deferral machinery itself over repeated cycles —
/// the bounded slot array must be returned, not consumed.
#[test]
fn repeated_pin_unlink_cycles_do_not_exhaust_the_deferral_list() {
    let _serial = pin_test_serial();
    let fs = mount_empty();
    for i in 0..64 {
        let name = alloc::format!("/cycle{i}");
        fs.write_file(&name, b"payload").unwrap();
        let inode = fs.resolve_inode(&name).unwrap();
        let pin = akuma_primitives::InodePin::new(inode);
        fs.remove_file(&name).unwrap();
        drop(pin);
    }
    // One more allocation to drain whatever the last cycle queued.
    fs.write_file("/drain", b"x").unwrap();

    assert_eq!(fs.deferred_free_len(), 0, "deferral list must drain");
    assert_eq!(
        crate::DEFERRED_FREE_LEAKED.load(core::sync::atomic::Ordering::Relaxed),
        0,
        "no inode should ever have been leaked at this scale",
    );
}

/// **Regression test for the ~148 MB/build on-disk leak.** Hold more inodes
/// pinned-and-unlinked *simultaneously* than the deferral list used to hold.
///
/// This is the shape `ext2probe`'s pinned phase measured on a live guest: 1200
/// files unlinked while a mapping held each one returned only 21 % of their
/// blocks, with `defer_leak` landing on exactly `1200 - 256` = **944**. The
/// cause was two defects at one line — `DEFERRED_FREE_SLOTS` was 256, and
/// `release_last_link` called `DeferredFrees::push` as a bare statement, so the
/// `false` meaning "no room, you must handle this" was discarded and the caller
/// zeroed `hard_links` anyway. See `EXT2_UNLINK_INODE_BLOCK_LEAK.md`.
///
/// 300 is chosen to sit **above the old 256 bound and far below the new one**,
/// so this test fails on the pre-fix code and passes after. All 300 pins are
/// held live across every unlink on purpose: releasing them one at a time would
/// let the list drain and never reach the cliff.
#[test]
fn pinned_unlinks_beyond_the_old_bound_leak_nothing() {
    let _serial = pin_test_serial();
    // NOT `mount_empty()`: `test.ext2` holds 256 inodes, the very bound under
    // test, so it runs out of inodes before it can overflow the list.
    let fs = mount_many_inodes();
    let leaked_before = crate::DEFERRED_FREE_LEAKED.load(core::sync::atomic::Ordering::Relaxed);

    const N: usize = 300;
    let mut pins = alloc::vec::Vec::with_capacity(N);
    let mut inodes = alloc::vec::Vec::with_capacity(N);
    for i in 0..N {
        let name = alloc::format!("/pinned{i}");
        fs.write_file(&name, b"payload-that-owns-a-block").unwrap();
        let inode = fs.resolve_inode(&name).unwrap();
        inodes.push(inode);
        // Pin BEFORE unlinking, and keep it: this is what forces the deferral.
        pins.push(akuma_primitives::InodePin::new(inode));
    }
    for i in 0..N {
        fs.remove_file(&alloc::format!("/pinned{i}")).unwrap();
    }

    // Every one of them must be queued, not abandoned.
    assert_eq!(
        fs.deferred_free_len(),
        N,
        "all {N} pinned unlinks must be queued; a smaller number means the list \
         overflowed and the surplus was leaked",
    );
    assert_eq!(
        crate::DEFERRED_FREE_LEAKED.load(core::sync::atomic::Ordering::Relaxed),
        leaked_before,
        "no inode may be leaked at {N} concurrent pinned unlinks",
    );

    // Drop the pins and give the drain something to run on.
    drop(pins);
    fs.write_file("/drain", b"x").unwrap();

    assert_eq!(fs.deferred_free_len(), 0, "the list must fully drain once unpinned");

    // Deliberately NOT a read-back of each freed inode. Freed inode *numbers*
    // are immediately reusable, and the `/drain` write above takes one — the
    // first attempt at this assertion failed on "inode 12 should have been
    // freed" because `/drain` had been allocated into inode 12 and legitimately
    // read back its 1 byte. The allocator racing the assertion says nothing
    // about the leak.
    //
    // What the leak actually is, and therefore what this test asserts, is above:
    // every pinned unlink was QUEUED (`deferred_free_len() == N`, which was
    // `256` before the fix), nothing was counted as leaked, and the queue
    // emptied once the pins dropped. Reuse of those inode numbers is itself
    // evidence the frees landed.
    let _ = inodes;
}

/// The read lever itself, measured deterministically instead of timed.
///
/// `Filesystem::read_at` re-resolves the path on **every** call — a full
/// `lookup_path_internal` walk plus a `read_inode` per component — which is
/// what `docs/archive/EXT2_WRITEBACK_DESIGN.md` § D-4 identified as the real
/// cost of a `read(2)` once the block cache had made the data itself warm.
/// `read_at_by_inode` skips all of it, and per-fd inode caching is what lets
/// `read(2)` use it.
///
/// Wall-clock cannot carry this claim on this host — the same probe has
/// measured the same commit 2x apart between sessions (`README.md`
/// § Performance) — but the work counts do not move at all between runs.
/// Measured here (5 components, 3000-byte file, 64 reads): **20 block-cache
/// accesses per read by path, 2 by inode**, and 64 tree walks against 0.
#[test]
fn reading_by_inode_does_no_path_walk() {
    let fs = mount_empty();
    fs.create_dir("/deep").unwrap();
    fs.create_dir("/deep/tree").unwrap();
    fs.create_dir("/deep/tree/of").unwrap();
    fs.create_dir("/deep/tree/of/dirs").unwrap();
    const PATH: &str = "/deep/tree/of/dirs/payload.bin";
    fs.write_file(PATH, &[0x5Au8; 3000]).unwrap();
    let inode = fs.resolve_inode(PATH).unwrap();

    // Warm every block both forms touch, so what follows is the steady-state
    // cost of a repeated read and not a cold-mount artifact.
    let mut buf = [0u8; 3000];
    fs.read_at(PATH, 0, &mut buf).unwrap();
    fs.read_at_by_inode(inode, 0, &mut buf).unwrap();

    const READS: u64 = 64;

    let (w0, b0) = fs.work_counters();
    for _ in 0..READS {
        assert_eq!(fs.read_at(PATH, 0, &mut buf).unwrap(), 3000);
    }
    let (w1, b1) = fs.work_counters();
    for _ in 0..READS {
        assert_eq!(fs.read_at_by_inode(inode, 0, &mut buf).unwrap(), 3000);
    }
    let (w2, b2) = fs.work_counters();

    let (path_walks, path_blocks) = (w1 - w0, b1 - b0);
    let (inode_walks, inode_blocks) = (w2 - w1, b2 - b1);

    assert_eq!(path_walks, READS, "read_at walks the tree once per call");
    assert_eq!(inode_walks, 0, "read_at_by_inode must never walk the tree");
    assert!(
        inode_blocks < path_blocks,
        "reading by inode must touch fewer blocks per read \
         (by path: {path_blocks} for {READS} reads, by inode: {inode_blocks})",
    );
    // The saving is the walk: one `read_inode` plus one directory-data read per
    // component, five components deep here. Asserted as a floor rather than an
    // exact figure so a future change to how a directory is read is a
    // measurement to re-take, not a test to silence.
    assert!(
        path_blocks - inode_blocks >= READS * 5,
        "a 5-component walk should cost at least one block access per component \
         (by path: {path_blocks}, by inode: {inode_blocks}, over {READS} reads)",
    );
}

/// `metadata_by_inode` must agree with `metadata` on a live file, and keep
/// answering on a pinned inode whose name is gone — the `stat` half of the same
/// guarantee `unlink_of_a_pinned_inode_keeps_its_data_readable` makes for reads.
///
/// Without it an unlinked-but-open fd could `read` perfectly well while `fstat`
/// on the same fd returned `ENOENT`: the fd knew which file it held and `stat`
/// did not.
#[test]
fn metadata_by_inode_matches_metadata_and_survives_unlink() {
    let _serial = pin_test_serial();
    let fs = mount_empty();
    fs.create_dir("/statdir").unwrap();
    fs.write_file("/statdir/f", &[7u8; 1234]).unwrap();
    let inode = fs.resolve_inode("/statdir/f").unwrap();

    let by_path = fs.metadata("/statdir/f").unwrap();
    let by_inode = fs.metadata_by_inode(inode).unwrap();
    assert_eq!(by_inode.size, 1234);
    assert_eq!(by_inode.inode, by_path.inode);
    assert_eq!(by_inode.size, by_path.size);
    assert_eq!(by_inode.mode, by_path.mode);
    assert_eq!(by_inode.is_dir, by_path.is_dir);

    // A directory reports itself as one, so `fstat` on a directory fd is right.
    let dir_inode = fs.resolve_inode("/statdir").unwrap();
    assert!(fs.metadata_by_inode(dir_inode).unwrap().is_dir);

    // The name goes; a reader still holds the inode. Size must survive, or
    // `lseek(SEEK_END)` on that fd silently treats the file as empty.
    let pin = akuma_primitives::InodePin::new(inode);
    fs.remove_file("/statdir/f").unwrap();
    assert!(fs.metadata("/statdir/f").is_err(), "the name must be gone");
    assert_eq!(
        fs.metadata_by_inode(inode).unwrap().size,
        1234,
        "a pinned inode must still report its real size after unlink",
    );
    drop(pin);
}

/// The gap the per-fd inode cache closed: `rename` unlinks its destination's
/// last name exactly as `remove_file` does, but only `remove_file` consulted the
/// pin. Atomic replace (`write foo.tmp; rename foo.tmp foo`) is what `cargo` and
/// `apk` do all day, so this was the *likeliest* way to pull an inode out from
/// under a live reader — an mmap region, or an fd opened before the rename.
#[test]
fn rename_over_a_pinned_inode_keeps_its_data_readable() {
    let _serial = pin_test_serial();
    let fs = mount_empty();
    fs.write_file("/config", b"THE OPEN READER'S BYTES").unwrap();
    let victim = fs.resolve_inode("/config").unwrap();
    let pin = akuma_primitives::InodePin::new(victim);

    fs.write_file("/config.tmp", b"replacement").unwrap();
    fs.rename("/config.tmp", "/config").unwrap();

    // The name now resolves to the replacement...
    assert_ne!(fs.resolve_inode("/config").unwrap(), victim);
    assert_eq!(fs.read_file("/config").unwrap(), b"replacement");
    // ...and the reader that opened the old one still reads the old one, in
    // full. Before the fix this read `Ok(0)` (i_size truncated to zero), and
    // after reissue it read the next file created.
    let mut buf = [0u8; 23];
    assert_eq!(
        fs.read_at_by_inode(victim, 0, &mut buf).unwrap(),
        23,
        "a pinned inode must not read short after being renamed over",
    );
    assert_eq!(&buf, b"THE OPEN READER'S BYTES");
    assert_eq!(fs.deferred_free_len(), 1, "the free must be queued, not lost");

    drop(pin);
}

/// The unchanged path: with nothing holding it, a renamed-over destination is
/// still freed on the spot, so the fix costs nothing on the common case.
#[test]
fn rename_over_an_unpinned_inode_frees_immediately() {
    let _serial = pin_test_serial();
    let fs = mount_empty();
    fs.write_file("/dst", b"doomed").unwrap();
    let victim = fs.resolve_inode("/dst").unwrap();
    fs.write_file("/src", b"winner").unwrap();

    fs.rename("/src", "/dst").unwrap();

    assert_eq!(fs.deferred_free_len(), 0, "nothing to defer");
    let mut buf = [0u8; 6];
    assert_eq!(
        fs.read_at_by_inode(victim, 0, &mut buf).unwrap(),
        0,
        "an unpinned destination is truncated and freed as before",
    );
    assert_eq!(fs.read_file("/dst").unwrap(), b"winner");
    assert!(!fs.exists("/src"), "the source name must be gone");
}

/// POSIX: renaming a name onto itself does nothing and succeeds. The old code
/// unlinked the shared inode, dropped its last link, freed it, then re-added a
/// dirent pointing at the freed number — `mv a a` destroyed the file.
#[test]
fn rename_onto_itself_keeps_the_file() {
    let fs = mount_empty();
    fs.write_file("/keepme", b"still here").unwrap();
    let inode = fs.resolve_inode("/keepme").unwrap();

    fs.rename("/keepme", "/keepme").unwrap();

    assert_eq!(fs.resolve_inode("/keepme").unwrap(), inode, "same inode");
    assert_eq!(fs.read_file("/keepme").unwrap(), b"still here");
}

/// A fast symlink stores its target *string* in `direct_blocks`, so truncating
/// one frees whatever block numbers those characters happen to spell.
/// `remove_file` guarded against that; `rename` reached the same truncate
/// without the guard. Stray frees show up as free blocks appearing from nowhere.
#[test]
fn rename_over_a_fast_symlink_frees_no_stray_blocks() {
    let fs = mount_empty();
    fs.create_symlink("/link", "some/relative/target.txt").unwrap();
    fs.write_file("/newfile", b"replaces the link").unwrap();
    let free_before = fs.stats().unwrap().free_blocks;

    fs.rename("/newfile", "/link").unwrap();

    assert_eq!(
        fs.stats().unwrap().free_blocks,
        free_before,
        "renaming over a fast symlink must free no data blocks at all",
    );
    assert_eq!(fs.read_file("/link").unwrap(), b"replaces the link");
}

/// `read(2)` on a directory fd reaches `read_at_by_inode` now that the fd
/// carries an inode, and must refuse exactly where the path-based `read_at`
/// does rather than handing back raw dirent bytes.
#[test]
fn read_at_by_inode_refuses_a_directory() {
    let fs = mount_empty();
    fs.create_dir("/somedir").unwrap();
    let inode = fs.resolve_inode("/somedir").unwrap();

    let mut buf = [0u8; 32];
    assert_eq!(
        fs.read_at_by_inode(inode, 0, &mut buf).unwrap_err(),
        akuma_vfs::FsError::NotAFile,
    );
    assert_eq!(
        fs.read_at("/somedir", 0, &mut buf).unwrap_err(),
        akuma_vfs::FsError::NotAFile,
        "and the two must agree",
    );
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

/// `append` reports where each write landed and never overwrites.
#[test]
fn append_lands_at_the_end_and_reports_the_offset() {
    let fs = mount_empty();
    assert_eq!(fs.append("/log", b"one\n").unwrap(), (0, 4), "creates the file");
    assert_eq!(fs.append("/log", b"two\n").unwrap(), (4, 4));
    assert_eq!(fs.read_file("/log").unwrap(), b"one\ntwo\n");
}

/// The `O_APPEND` race, on the host: two threads appending lines to one file.
/// Picking the offset outside the state lock (`metadata` then `write_at`, the
/// trait's default and what the syscall layer did) loses lines — 298 of 300 on
/// the trashcan from two shell `>>` loops
/// (`docs/archive/AKUMA_AMD64_EXT2_CROSS_FILE_CORRUPTION.md` §5). `append`
/// must keep every one, intact and unsplit.
#[test]
fn concurrent_appends_lose_nothing() {
    let fs = mount_empty();
    fs.write_file("/log", b"").unwrap();
    std::thread::scope(|s| {
        for t in 0..2u8 {
            let fs = &fs;
            s.spawn(move || {
                for i in 0..150 {
                    let line = alloc::format!("{t} {i:03} ..........................................\n");
                    let (_, n) = fs.append("/log", line.as_bytes()).unwrap();
                    assert_eq!(n, line.len());
                }
            });
        }
    });
    let data = fs.read_file("/log").unwrap();
    let text = core::str::from_utf8(&data).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 300, "every appended line survives");
    for t in 0..2 {
        for i in 0..150 {
            let want = alloc::format!("{t} {i:03} ..........................................");
            assert!(lines.contains(&want.as_str()), "missing or torn: {want}");
        }
    }
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
// Write-back cache tests (design doc: docs/archive/EXT2_WRITEBACK_DESIGN.md)
// ============================================================================

use core::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

/// Shared in-memory device that records every device read/write. The `Arc`
/// inner lets a test open a *second* `Ext2Filesystem` over the same bytes —
/// the persistence oracle: after an op returns (or `sync()`), what the device
/// holds must be a complete, self-consistent filesystem.
struct RecordingDevice {
    inner: spinning_top::Spinlock<Vec<u8>>,
    reads: AtomicU64,
    writes: AtomicU64,
    /// `(offset, len)` of every write, in order — for flush-ordering asserts.
    write_log: spinning_top::Spinlock<alloc::vec::Vec<(u64, usize)>>,
    /// How many times `flush` (the media barrier) was asked for.
    flushes: AtomicU64,
}

impl RecordingDevice {
    fn from_fixture(name: &str) -> Self {
        let path = alloc::format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name);
        extern crate std;
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("failed to read fixture {path}: {e}"));
        Self {
            inner: spinning_top::Spinlock::new(bytes),
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            write_log: spinning_top::Spinlock::new(alloc::vec::Vec::new()),
            flushes: AtomicU64::new(0),
        }
    }

    fn read_count(&self) -> u64 {
        self.reads.load(AtomicOrdering::Relaxed)
    }
}

/// Mounted through `&RecordingDevice` so one device (and its counters/log)
/// can back two filesystem instances — the persistence-oracle tests.
impl BlockDevice for &RecordingDevice {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        (**self).read_bytes(offset, buf)
    }

    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
        (**self).write_bytes(offset, data)
    }

    fn flush(&self) -> Result<(), ()> {
        (**self).flush()
    }
}

impl BlockDevice for RecordingDevice {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        self.reads.fetch_add(1, AtomicOrdering::Relaxed);
        let data = self.inner.lock();
        let off = offset as usize;
        if off + buf.len() > data.len() {
            return Err(());
        }
        buf.copy_from_slice(&data[off..off + buf.len()]);
        Ok(())
    }

    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
        self.writes.fetch_add(1, AtomicOrdering::Relaxed);
        self.write_log.lock().push((offset, data.len()));
        let mut inner = self.inner.lock();
        let off = offset as usize;
        if off + data.len() > inner.len() {
            return Err(());
        }
        inner[off..off + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn flush(&self) -> Result<(), ()> {
        self.flushes.fetch_add(1, AtomicOrdering::Relaxed);
        Ok(())
    }
}

/// Read-after-write must not re-read the file's **data** blocks: write-back
/// keeps them dirty-resident, so the read-back's device reads are limited to
/// *metadata* re-fills (inode table, BGD, dirent block) that the small 64-slot
/// ring may have evicted mid-op. The old write-through cache paid a cold
/// device read per data block on top of that — 4 for this file. What this
/// asserts: data blocks contribute zero, i.e. reads ≤ the metadata working
/// set, and — the sharp version, under `fs-cache` where nothing evicts —
/// exactly zero.
#[test]
fn writeback_read_after_write_is_warm() {
    let dev = RecordingDevice::from_fixture("test.ext2");
    let fs = Ext2Filesystem::new(&dev, || 0).unwrap();
    fs.write_file("/warm.bin", &[0xA5u8; 4096]).unwrap();

    let reads_before = dev.read_count();
    let got = fs.read_file("/warm.bin").unwrap();
    assert_eq!(got.len(), 4096);
    assert!(got.iter().all(|&b| 0xA5 == b));
    let reads = dev.read_count() - reads_before;
    // 4 data blocks + ≤9 metadata blocks (superblock uncached, BGD ×groups,
    // inode table, bitmaps, dirent). Anything above that means data blocks
    // came back from the device — the write-through behavior we replaced.
    assert!(
        reads <= 9,
        "read-after-write performed {reads} device reads; data blocks must be cache hits (write-through residue?)"
    );
    #[cfg(feature = "fs-cache")]
    assert_eq!(reads, 0, "fs-cache is big enough that nothing evicts; expected zero reads");
}

/// Every mutating op ends in `flush_meta`, so when the op returns the device
/// already holds the data — verified by mounting a *fresh* filesystem over the
/// same device bytes and reading the file back (the persistence oracle).
#[test]
fn writeback_data_reaches_device_by_op_end() {
    let dev = RecordingDevice::from_fixture("test.ext2");
    let fs = Ext2Filesystem::new(&dev, || 0).unwrap();
    fs.write_file("/persist.bin", b"WRITEBACK PERSISTED").unwrap();

    let fs2 = Ext2Filesystem::new(&dev, || 0).unwrap();
    assert_eq!(
        fs2.read_file("/persist.bin").unwrap(),
        b"WRITEBACK PERSISTED",
        "flush_meta must have pushed dirty data to the device before the op returned"
    );
}

/// The one real correctness hazard of write-back (design doc D-3): a freed
/// block's stale dirty copy must be *dropped*, never flushed nor served. Poison
/// recipe: fill block with junk, delete (free), reallocate the same block, do a
/// **partial** write — the un-written tail must read back as zeros, not junk.
/// (The partial write's read-back would hit the stale cached copy if
/// invalidate-on-free were missing.)
#[test]
fn writeback_free_realloc_has_no_stale_bytes() {
    let dev = RecordingDevice::from_fixture("test.ext2");
    let fs = Ext2Filesystem::new(&dev, || 0).unwrap();

    // File A: one full block of junk.
    fs.write_file("/a.junk", &[0xEEu8; 4096]).unwrap();
    fs.remove_file("/a.junk").unwrap();

    // File B reuses the block; only byte 0 is written.
    fs.write_at("/b.fresh", 0, &[0x42]).unwrap();
    let got = fs.read_file("/b.fresh").unwrap();
    assert_eq!(got.len(), 1, "size must be 1 byte");

    // Now extend with a partial write past the block start and read the tail.
    fs.write_at("/b.fresh", 8, &[0x99]).unwrap();
    let got = fs.read_file("/b.fresh").unwrap();
    assert_eq!(got.len(), 9);
    assert_eq!(got[0], 0x42);
    assert_eq!(got[8], 0x99);
    for (i, &b) in got[1..8].iter().enumerate() {
        assert_eq!(b, 0, "byte {} of the reallocated block must be zero, got {:#x}", i + 1, b);
    }

    // And the device copy must agree (no junk ever flushed).
    let fs2 = Ext2Filesystem::new(&dev, || 0).unwrap();
    let got2 = fs2.read_file("/b.fresh").unwrap();
    assert_eq!(got, got2, "device and cache must agree after flush");
}

/// Flush ordering (design doc D-2): within one mutating op, the file's data
/// block must reach the device *before* the superblock (phase 2 metadata). A
/// crash between the two leaks an allocated block (e2fsck-recoverable) instead
/// of publishing free counts that disagree with the data.
#[test]
fn writeback_flushes_data_before_superblock() {
    let dev = RecordingDevice::from_fixture("test.ext2");
    let fs = Ext2Filesystem::new(&dev, || 0).unwrap();
    fs.write_file("/order.bin", b"ORDER-MAGIC-1234").unwrap();

    let log = dev.write_log.lock();
    let inner = dev.inner.lock();
    let data_idx = log
        .iter()
        .position(|&(off, len)| {
            let start = off as usize;
            if start + len > inner.len() {
                return false;
            }
            inner[start..start + len].windows(4).any(|w| w == b"RDER" || w == b"MAGI" || w == b"1234")
        })
        .expect("the file data block must have been written to the device");
    let sb_idx = log
        .iter()
        .rposition(|&(off, _)| off == 1024)
        .expect("the superblock must have been written (free counts changed)");
    assert!(
        data_idx < sb_idx,
        "data block write (log[{data_idx}]) must precede the superblock write (log[{sb_idx}])"
    );
}

/// Dirty-ring eviction must flush victims (design doc D-1): write far more
/// blocks than the 64-slot ring holds, then verify from a fresh mount that
/// every byte landed. This drives the run through `alloc_slot`'s flush path.
#[test]
fn writeback_ring_eviction_flushes_dirty_victims() {
    let dev = RecordingDevice::from_fixture("test.ext2");
    let fs = Ext2Filesystem::new(&dev, || 0).unwrap();
    // ~150 KB across blocks well past the ring's 64 slots (block size 1024).
    let mut pattern = alloc::vec![0u8; 150 * 1024];
    for (i, b) in pattern.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    fs.write_file("/big.bin", &pattern).unwrap();

    let fs2 = Ext2Filesystem::new(&dev, || 0).unwrap();
    let got = fs2.read_file("/big.bin").unwrap();
    assert_eq!(got.len(), pattern.len());
    let first_bad = got.iter().zip(pattern.iter()).position(|(a, b)| a != b);
    assert!(
        first_bad.is_none(),
        "evicted dirty blocks lost data, first divergence at {first_bad:?}"
    );
}

/// Bitmap-scan cursor correctness (design doc D-6): fragmentation must not
/// make the allocator miss free bits. Fill, punch holes, refill — every
/// allocation must succeed and the reopened device must agree.
#[test]
fn writeback_fragmented_allocation_finds_holes() {
    let dev = RecordingDevice::from_fixture("test.ext2");
    let fs = Ext2Filesystem::new(&dev, || 0).unwrap();

    for i in 0..24 {
        let name = alloc::format!("/frag{i}.dat");
        let fill = [0xF0u8 ^ (i as u8); 2048];
        fs.write_file(&name, &fill).unwrap();
    }
    // Punch holes: delete every even file.
    for i in (0..24).step_by(2) {
        let name = alloc::format!("/frag{i}.dat");
        fs.remove_file(&name).unwrap();
    }
    // Refill with new files — the cursor must find the freed bits (wrapping
    // past them or being pulled back), never report NoSpace early.
    for i in 0..12 {
        let name = alloc::format!("/refill{i}.dat");
        let fill = [0x0Fu8 | (i as u8); 2048];
        fs.write_file(&name, &fill).unwrap();
    }

    // Oracle: fresh mount over the device bytes.
    let fs2 = Ext2Filesystem::new(&dev, || 0).unwrap();
    for i in (1..24).step_by(2) {
        let name = alloc::format!("/frag{i}.dat");
        let want = [0xF0u8 ^ (i as u8); 2048];
        assert_eq!(fs2.read_file(&name).unwrap(), want, "{name} corrupted");
    }
    for i in 0..12 {
        let name = alloc::format!("/refill{i}.dat");
        let want = [0x0Fu8 | (i as u8); 2048];
        assert_eq!(fs2.read_file(&name).unwrap(), want, "{name} corrupted");
    }
}

/// Mixed adversarial workload + persistence oracle: create/extend/truncate/
/// rename/delete interleaved, then a fresh mount must see exactly the
/// surviving set with the right contents. This is the host stand-in for the
/// E2C-BAD coherence hunt: cache-ahead-of-disk is fine *until an op ends*;
/// after that, device bytes are the truth.
#[test]
fn writeback_mixed_workload_persists_coherently() {
    let dev = RecordingDevice::from_fixture("test.ext2");
    let fs = Ext2Filesystem::new(&dev, || 0).unwrap();

    fs.create_dir("/mix").unwrap();
    for i in 0..10 {
        let name = alloc::format!("/mix/f{i}");
        let body = alloc::vec![(i * 7 % 256) as u8; 1024 + i * 100];
        fs.write_file(&name, &body).unwrap();
    }
    // Extend a couple (partial blocks both ends).
    fs.write_at("/mix/f3", 500, &[0xAB; 300]).unwrap();
    fs.write_at("/mix/f7", 1400, &[0xCD; 50]).unwrap();
    // Truncate-by-rewrite one.
    fs.write_file("/mix/f5", b"tiny now").unwrap();
    // Rename one.
    fs.rename("/mix/f9", "/mix/f9-renamed").unwrap();
    // Delete some. Stepping by 4 (f0/f4/f8) deliberately spares f3/f5/f7 — the
    // three the mutation phase above touched, whose patched contents are the
    // point of the oracle below.
    for i in (0..9).step_by(4) {
        let name = alloc::format!("/mix/f{i}");
        fs.remove_file(&name).unwrap();
    }
    fs.sync().unwrap();

    let fs2 = Ext2Filesystem::new(&dev, || 0).unwrap();
    for i in 0..10 {
        let name = if i == 9 {
            alloc::string::String::from("/mix/f9-renamed")
        } else {
            alloc::format!("/mix/f{i}")
        };
        if i % 4 == 0 {
            assert!(!fs2.exists(&name), "{name} should be deleted");
            continue;
        }
        let got = fs2.read_file(&name).unwrap();
        // The writer's fill, plus whatever the mutation phase patched into it.
        let want: alloc::vec::Vec<u8> = if i == 5 {
            b"tiny now".to_vec()
        } else {
            let mut v = alloc::vec![(i * 7 % 256) as u8; 1024 + i * 100];
            if i == 3 {
                v[500..800].copy_from_slice(&[0xAB; 300]);
            }
            if i == 7 {
                v[1400..1450].copy_from_slice(&[0xCD; 50]);
            }
            v
        };
        assert_eq!(got, want, "{name} content mismatch after mixed workload");
    }
}

/// The `[E2C-BAD]` coherence oracle as a host test (design doc §"Race-safety
/// argument"): flip `E2_VERIFY_HITS` on, run a mixed adversarial workload, and
/// require every cache hit to match a direct device re-read. Under write-back
/// a dirty hit legitimately differs, so `verify_cached_block` skips dirty
/// blocks — what this hunts is exactly "clean slot ≠ disk". The global knob is
/// process-wide, so the test takes the serial guard; a mismatch aborts the
/// whole suite loudly either way.
#[test]
fn writeback_coherence_oracle_zero_mismatches() {
    use crate::ext2::{E2_CACHE_VERIFY_MISMATCH, E2_VERIFY_HITS};
    let _serial = pin_test_serial();
    let prev = E2_VERIFY_HITS.swap(true, AtomicOrdering::SeqCst);
    E2_CACHE_VERIFY_MISMATCH.store(0, AtomicOrdering::SeqCst);

    let dev = RecordingDevice::from_fixture("test.ext2");
    let fs = Ext2Filesystem::new(&dev, || 0).unwrap();

    fs.create_dir("/oracle").unwrap();
    for i in 0..8 {
        let name = alloc::format!("/oracle/w{i}");
        let body = alloc::vec![0xC0 ^ (i as u8); 3 * 1024 + i * 37];
        fs.write_file(&name, &body).unwrap();
        // Interleave reads (hits under verification) with more writes.
        let _ = fs.read_file(&name).unwrap();
    }
    // Rewrite some blocks in place (write-then-read coherence).
    fs.write_at("/oracle/w2", 512, &[0x11; 600]).unwrap();
    let _ = fs.read_file("/oracle/w2").unwrap();
    // Free/realloc churn while verifying.
    fs.remove_file("/oracle/w4").unwrap();
    fs.write_file("/oracle/after4", &[0x77; 1024]).unwrap();
    let _ = fs.read_file("/oracle/after4").unwrap();
    fs.sync().unwrap();
    // Post-sync: every slot is clean, so every hit is now unconditionally
    // verified against the device.
    for i in 0..8 {
        if i == 4 {
            continue;
        }
        let name = alloc::format!("/oracle/w{i}");
        let _ = fs.read_file(&name).unwrap();
    }

    let mismatches = E2_CACHE_VERIFY_MISMATCH.load(AtomicOrdering::SeqCst);
    E2_VERIFY_HITS.store(prev, AtomicOrdering::SeqCst);
    assert_eq!(
        mismatches, 0,
        "E2C-BAD: {mismatches} cache hits served bytes the disk does not have"
    );
}

// ============================================================================
// ClockBlockCache (large block cache, feature `fs-cache`) unit tests.
// Compiled whenever `cfg(test)` is active (the cache type is `cfg(any(ext2_fs_cache, test))`).
// ============================================================================

use crate::ext2::{ClockBlockCache, cache_stats, set_cache_cap_bytes};

/// No-op device flush for cache unit tests (nothing is ever dirty-clean evicted
/// with wrong bytes here; tests that care assert on the flush sequence).
fn noop_flush() -> impl FnMut(u32, &[u8]) -> Result<(), akuma_vfs::FsError> {
    |_bn, _bytes| Ok(())
}

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
    c.insert(5, &blk(5, bs), &mut noop_flush()).unwrap();
    let got = c.get(5).expect("inserted block must hit");
    assert_eq!(&got[0..4], &5u32.to_le_bytes(), "wrong block data returned");
}

#[test]
fn clock_cache_dedup_insert() {
    let bs = 1024;
    let mut c = ClockBlockCache::with_capacity_blocks(bs, 8);
    c.insert(7, &blk(7, bs), &mut noop_flush()).unwrap();
    c.insert(7, &blk(7, bs), &mut noop_flush()).unwrap(); // duplicate: must not create a second slot
    // Fill the rest; if the dup created a slot we'd evict 7 one round early.
    for n in 100..107 {
        c.insert(n, &blk(n, bs), &mut noop_flush()).unwrap();
    }
    assert!(c.get(7).is_some(), "block 7 should still be resident (no dup slot)");
}

#[test]
fn clock_cache_remove_invalidates() {
    let bs = 512;
    let mut c = ClockBlockCache::with_capacity_blocks(bs, 8);
    c.insert(3, &blk(3, bs), &mut noop_flush()).unwrap();
    assert!(c.get(3).is_some());
    c.remove(3);
    assert!(c.get(3).is_none(), "removed block must miss");
    // The freed slot must be reusable.
    c.insert(9, &blk(9, bs), &mut noop_flush()).unwrap();
    assert!(c.get(9).is_some(), "freed slot must be reusable");
}

#[test]
fn clock_cache_second_chance_spares_referenced_block() {
    // The defining property of clock vs a pure ring: a *referenced* block gets a
    // second chance and survives an eviction in favour of an unreferenced one.
    let bs = 256;
    let mut c = ClockBlockCache::with_capacity_blocks(bs, 4);
    for n in 0..4 {
        c.insert(n, &blk(n, bs), &mut noop_flush()).unwrap(); // slots 0..3, all ref=1, hand=0
    }
    // Full + all bits set => first eviction is FIFO (block 0). This is correct
    // clock behaviour, not a bug — every block had its chance.
    c.insert(4, &blk(4, bs), &mut noop_flush()).unwrap();
    assert!(c.get(0).is_none(), "block 0 should be evicted (FIFO when all referenced)");
    // Now present: 4,1,2,3 with ref=[1,0,0,0]. Touch 1 and 2; leave 3 cold.
    assert!(c.get(1).is_some());
    assert!(c.get(2).is_some());
    // Insert 5: the hand clears 1 and 2 (second chance) and evicts the cold 3.
    c.insert(5, &blk(5, bs), &mut noop_flush()).unwrap();
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
        c.insert(n, &blk(n, bs), &mut noop_flush()).unwrap();
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

// ── ClockBlockCache write-back (dirty bit, D-1/D-3/D-5) ─────────────

/// `write` defers the device write: dirty immediately, device pushed only by
/// `flush_dirty` (and exactly once).
#[test]
fn clock_cache_write_defers_until_flush() {
    let bs = 1024;
    let mut c = ClockBlockCache::with_capacity_blocks(bs, 8);
    let flushed: alloc::rc::Rc<core::cell::RefCell<alloc::vec::Vec<u32>>> =
        alloc::rc::Rc::new(core::cell::RefCell::new(alloc::vec::Vec::new()));
    let mut sink = {
        let flushed = alloc::rc::Rc::clone(&flushed);
        move |bn: u32, _: &[u8]| -> Result<(), akuma_vfs::FsError> {
            flushed.borrow_mut().push(bn);
            Ok(())
        }
    };
    c.write(5, &blk(5, bs), &mut sink).unwrap();
    assert!(c.is_dirty(5), "written block must be dirty before flush");
    assert!(flushed.borrow().is_empty(), "write itself must not touch the device");
    // data is already readable through the cache
    assert_eq!(&c.get(5).unwrap()[0..4], &5u32.to_le_bytes());

    c.flush_dirty(&|_| true, &mut sink).unwrap();
    assert!(!c.is_dirty(5), "flush must clear the dirty bit");
    assert_eq!(&*flushed.borrow(), &[5], "flush must push exactly the dirty block");

    // Second flush is a no-op.
    c.flush_dirty(&|_| true, &mut sink).unwrap();
    assert_eq!(flushed.borrow().len(), 1, "clean blocks must not be re-flushed");
}

/// Evicting a dirty victim flushes it first (the D-1 eviction path).
#[test]
fn clock_cache_dirty_eviction_flushes_victim() {
    let bs = 256;
    let mut c = ClockBlockCache::with_capacity_blocks(bs, 2);
    let flushed: alloc::rc::Rc<core::cell::RefCell<alloc::vec::Vec<u32>>> =
        alloc::rc::Rc::new(core::cell::RefCell::new(alloc::vec::Vec::new()));
    let mut sink = {
        let flushed = alloc::rc::Rc::clone(&flushed);
        move |bn: u32, _: &[u8]| -> Result<(), akuma_vfs::FsError> {
            flushed.borrow_mut().push(bn);
            Ok(())
        }
    };
    c.write(1, &blk(1, bs), &mut sink).unwrap();
    c.write(2, &blk(2, bs), &mut sink).unwrap();
    // Third write forces eviction of a dirty victim; capacity 2, both dirty.
    c.write(3, &blk(3, bs), &mut sink).unwrap();
    assert!(
        flushed.borrow().contains(&1) || flushed.borrow().contains(&2),
        "evicting a dirty victim must flush it (flushed so far: {:?})",
        flushed.borrow()
    );
    assert!(flushed.borrow().iter().all(|&b| b != 3), "the new block must stay dirty in cache");
}

/// `patch` overwrites a sub-range of a resident block and marks it dirty —
/// the inode-table / BGD fast path (design doc D-5).
#[test]
fn clock_cache_patch_dirties_resident_block() {
    let bs = 1024;
    let mut c = ClockBlockCache::with_capacity_blocks(bs, 4);
    let mut sink = |_bn: u32, _: &[u8]| -> Result<(), akuma_vfs::FsError> { Ok(()) };
    c.insert(9, &blk(9, bs), &mut sink).unwrap();
    assert!(!c.is_dirty(9));

    assert!(c.patch(9, 64, &[0xDE, 0xAD, 0xBE, 0xEF]), "resident block must be patchable");
    assert!(c.is_dirty(9), "patch must mark the block dirty");
    assert_eq!(&c.get(9).unwrap()[64..68], &[0xDE, 0xAD, 0xBE, 0xEF]);
    assert_eq!(&c.get(9).unwrap()[0..4], &9u32.to_le_bytes(), "bytes before the patch must survive");

    assert!(!c.patch(12345, 0, &[1]), "absent block must report unpatched");
}

/// `remove` drops a dirty block silently (invalidate-on-free, D-3): no flush,
/// no dirty residue, slot reusable.
#[test]
fn clock_cache_remove_drops_dirty_silently() {
    let bs = 1024;
    let mut c = ClockBlockCache::with_capacity_blocks(bs, 4);
    let flushed: alloc::rc::Rc<core::cell::RefCell<alloc::vec::Vec<u32>>> =
        alloc::rc::Rc::new(core::cell::RefCell::new(alloc::vec::Vec::new()));
    let mut sink = {
        let flushed = alloc::rc::Rc::clone(&flushed);
        move |bn: u32, _: &[u8]| -> Result<(), akuma_vfs::FsError> {
            flushed.borrow_mut().push(bn);
            Ok(())
        }
    };
    c.write(6, &blk(6, bs), &mut sink).unwrap();
    assert!(c.is_dirty(6));
    c.remove(6);
    assert!(c.get(6).is_none(), "removed block must miss");
    assert!(!c.is_dirty(6), "removed block must not be dirty");
    c.flush_dirty(&|_| true, &mut sink).unwrap();
    assert!(
        flushed.borrow().is_empty(),
        "a freed block's bytes must never be flushed (D-3), flushed: {:?}",
        flushed.borrow()
    );
}

/// `flush_dirty`'s `keep` filter implements the D-2 ordering: only blocks the
/// filter admits are pushed, the rest stay dirty for a later phase.
#[test]
fn clock_cache_flush_dirty_respects_keep_filter() {
    let bs = 512;
    let mut c = ClockBlockCache::with_capacity_blocks(bs, 8);
    let flushed: alloc::rc::Rc<core::cell::RefCell<alloc::vec::Vec<u32>>> =
        alloc::rc::Rc::new(core::cell::RefCell::new(alloc::vec::Vec::new()));
    let mut sink = {
        let flushed = alloc::rc::Rc::clone(&flushed);
        move |bn: u32, _: &[u8]| -> Result<(), akuma_vfs::FsError> {
            flushed.borrow_mut().push(bn);
            Ok(())
        }
    };
    c.write(10, &blk(10, bs), &mut sink).unwrap();
    c.write(11, &blk(11, bs), &mut sink).unwrap();

    c.flush_dirty(&|bn| bn != 11, &mut sink).unwrap();
    assert_eq!(&*flushed.borrow(), &[10], "only admitted blocks are flushed");
    assert!(!c.is_dirty(10), "flushed block is clean");
    assert!(c.is_dirty(11), "filtered-out block stays dirty");

    c.flush_dirty(&|_| true, &mut sink).unwrap();
    assert_eq!(&*flushed.borrow(), &[10, 11], "the second phase picks up the rest");
}

// ============================================================================
// On-disk layout codec (docs/archive/AKUMA_EXT2_CLEANUP.md §2.2, §5 step 2)
// ============================================================================

use crate::ext2::{
    BlockGroupDescriptor, DirEntryRaw, Inode, Superblock, DIR_ENTRY_HEADER_SIZE,
    FAST_SYMLINK_MAX, INODE_POINTERS_OFFSET,
};

/// A superblock with a distinct value in every field, so a round-trip catches
/// a wrong offset (two swapped fields of the same width cannot both survive).
fn sample_superblock() -> Superblock {
    Superblock {
        total_inodes: 0x0102_0304,
        total_blocks: 0x1112_1314,
        superuser_blocks: 5,
        unallocated_blocks: 0x2122_2324,
        unallocated_inodes: 0x3132_3334,
        first_data_block: 1,
        block_size_log: 0,
        fragment_size_log: 0,
        blocks_per_group: 8192,
        fragments_per_group: 8192,
        inodes_per_group: 2048,
        last_mount_time: 1_000,
        last_written_time: 2_000,
        mount_count: 3,
        max_mount_count: 20,
        magic: 0xEF53,
        fs_state: 1,
        error_handling: 1,
        version_minor: 4,
        last_check_time: 3_000,
        check_interval: 18_000,
        creator_os: 0,
        version_major: 1,
        reserved_uid: 0,
        reserved_gid: 0,
        first_inode: 11,
        inode_size: 128,
        block_group: 0,
        feature_compat: 0x4,
        feature_incompat: 0x1,
        feature_ro_compat: 0x2,
        uuid: [0xA5; 16],
        volume_name: *b"akuma-test-image",
        last_mounted: [b'/'; 64],
        algo_bitmap: 7,
        _padding: core::array::from_fn(|i| (i % 251) as u8),
    }
}

/// `parse(serialize(x)) == x` for the superblock — the test a misplaced offset
/// fails instead of a live disk. The padding is part of the contract: a
/// write-back must be byte-faithful to what was read, reserved bytes included.
#[test]
fn superblock_roundtrip() {
    let sb = sample_superblock();
    let mut buf = [0u8; Superblock::SIZE];
    buf.fill(0xEE); // serialize must overwrite every byte, padding included
    sb.serialize(&mut buf);
    assert_eq!(Superblock::parse(&buf), Some(sb));
}

/// The magic the mount path checks sits at the spec's offset 56 — parse and
/// the fixture images agree, which the mount tests exercise end to end.
#[test]
fn superblock_parse_reads_fixture_magic() {
    let dev = load_fixture("test.ext2");
    let mut sb_buf = [0u8; Superblock::SIZE];
    dev.read_bytes(1024, &mut sb_buf).unwrap();
    let sb = Superblock::parse(&sb_buf).expect("fixture superblock parses");
    assert_eq!(sb.magic, 0xEF53);
    assert_eq!(sb.block_size_log, 0, "fixture is a 1 KiB-block image");
}

#[test]
fn superblock_parse_rejects_short_buffer() {
    let sb = sample_superblock();
    let mut buf = [0u8; Superblock::SIZE];
    sb.serialize(&mut buf);
    assert_eq!(Superblock::parse(&buf[..1023]), None);
}

fn sample_bgd() -> BlockGroupDescriptor {
    BlockGroupDescriptor {
        block_bitmap: 0x0102_0304,
        inode_bitmap: 0x1112_1314,
        inode_table: 0x2122_2324,
        free_blocks_count: 0x3132,
        free_inodes_count: 0x4142,
        used_dirs_count: 0x5152,
        _padding: 0,
        _reserved: [0; 12],
    }
}

#[test]
fn bgd_roundtrip() {
    let bgd = sample_bgd();
    let mut buf = [0u8; BlockGroupDescriptor::SIZE];
    buf.fill(0xEE);
    bgd.serialize(&mut buf);
    assert_eq!(BlockGroupDescriptor::parse(&buf), Some(bgd));
    assert_eq!(BlockGroupDescriptor::parse(&buf[..31]), None);
}

fn sample_inode() -> Inode {
    Inode {
        type_perms: 0o100_644,
        uid: 1000,
        size_lower: 0x0102_0304,
        access_time: 1_111,
        creation_time: 2_222,
        modification_time: 3_333,
        deletion_time: 0,
        gid: 1001,
        hard_links: 2,
        sectors_used: 0x4142_4344,
        flags: 0x5152_5354,
        os_specific_1: 0x6162_6364,
        direct_blocks: core::array::from_fn(|i| 100 + i as u32),
        indirect_block: 0x7172_7374,
        double_indirect_block: 0x8182_8384,
        triple_indirect_block: 0x9192_9394,
        generation: 0xA1A2_A3A4,
        file_acl: 0xB1B2_B3B4,
        size_upper: 0xC1C2_C3C4,
        fragment_addr: 0xD1D2_D3D4,
        os_specific_2: [0x5A; 12],
    }
}

#[test]
fn inode_roundtrip() {
    let inode = sample_inode();
    let mut buf = [0u8; Inode::SIZE];
    buf.fill(0xEE);
    inode.serialize(&mut buf);
    assert_eq!(Inode::parse(&buf), Some(inode));
    assert_eq!(Inode::parse(&buf[..127]), None);
}

/// A larger on-disk inode (rev-1 filesystems commonly use 256) parses from its
/// first 128 bytes — the tail is other people's data and must not be read.
#[test]
fn inode_parse_reads_only_the_first_128_bytes_of_a_big_entry() {
    let inode = sample_inode();
    let mut entry = [0u8; 256];
    entry[128..].fill(0x99);
    inode.serialize(&mut entry[..Inode::SIZE]);
    assert_eq!(Inode::parse(&entry), Some(inode));
}

/// The fast-symlink window is bytes 40..100 of the *serialized* inode — all
/// 15 pointer words, not just `direct_blocks` (§3). Stopping at 48 bytes would
/// cap targets at 48 and break on-disk Linux compatibility.
#[test]
fn fast_symlink_target_is_bytes_40_to_100_of_the_serialized_inode() {
    let target: Vec<u8> = (0..FAST_SYMLINK_MAX).map(|i| b"0123456789abcdef"[i % 16]).collect();
    assert_eq!(target.len(), 60);

    let mut inode = Inode::default();
    inode.set_fast_symlink_target(&target);

    let mut buf = [0u8; Inode::SIZE];
    inode.serialize(&mut buf);
    assert_eq!(&buf[INODE_POINTERS_OFFSET..INODE_POINTERS_OFFSET + 60], &target[..]);

    assert_eq!(&inode.fast_symlink_target(60), target.as_slice());
}

/// Exactly 60 bytes works (the maximum), and the untouched window tail reads
/// back as zeros so a shorter target cannot smuggle stale pointer bytes.
#[test]
fn fast_symlink_window_beyond_the_target_is_zeroed() {
    let mut inode = Inode::default();
    inode.set_fast_symlink_target(b"short");
    let raw = inode.fast_symlink_target(FAST_SYMLINK_MAX);
    assert_eq!(&raw[..5], b"short");
    assert!(raw[5..].iter().all(|&b| b == 0));

    // And a fresh target overwrites a previous one completely.
    inode.set_fast_symlink_target(&[0xFF; FAST_SYMLINK_MAX]);
    let raw = inode.fast_symlink_target(FAST_SYMLINK_MAX);
    assert!(raw.iter().all(|&b| b == 0xFF));
}

fn sample_dirent() -> DirEntryRaw {
    DirEntryRaw { inode: 0x0102_0304, rec_len: 0x1112, name_len: 0x13, file_type: 2 }
}

#[test]
fn dirent_roundtrip() {
    let entry = sample_dirent();
    let mut buf = [0u8; DIR_ENTRY_HEADER_SIZE];
    buf.fill(0xEE);
    entry.serialize(&mut buf);
    assert_eq!(DirEntryRaw::parse(&buf), Some(entry));
    assert_eq!(DirEntryRaw::parse(&buf[..7]), None);
}

/// A real directory entry's `rec_len` usually exceeds header + name (entries
/// are padded toward the next 4-byte boundary, and the last one in a block
/// absorbs the rest). The header parses identically whatever the padding.
#[test]
fn dirent_with_padded_rec_len_roundtrips() {
    let entry = DirEntryRaw { inode: 42, rec_len: 1004, name_len: 3, file_type: 1 };
    let mut dir_data = vec![0u8; 1024];
    entry.serialize(&mut dir_data);
    dir_data[DIR_ENTRY_HEADER_SIZE..DIR_ENTRY_HEADER_SIZE + 3].copy_from_slice(b"abc");
    // rec_len 1004 covers the rest of the block; the name bytes beyond "abc"
    // are padding garbage and must not affect the parsed header.
    dir_data[DIR_ENTRY_HEADER_SIZE + 3] = 0xAA;

    let parsed = DirEntryRaw::parse(&dir_data).expect("full-block dirent parses");
    assert_eq!(parsed, entry);
}

// ── Mount-path validation of disk-supplied arithmetic (§2.3) ────────────────
//
// Each of these crafts a corrupt-but-magic-matching image by patching one
// superblock field of the fixture. Before §2.3 these panicked at mount (÷0,
// shift overflow), wrapped into garbage (the block_group_count underflow), or
// read past a heap allocation (inode_size < 128). A filesystem driver must
// reject them, not die on them.

/// Patch one little-endian superblock field in a copy of the fixture image.
fn fixture_with_sb_field(offset: u64, bytes: &[u8]) -> MemBlockDevice {
    let path = alloc::format!(
        "{}/tests/fixtures/test.ext2",
        env!("CARGO_MANIFEST_DIR")
    );
    extern crate std;
    let mut image = std::fs::read(&path).expect("read fixture");
    let at = 1024 + offset as usize;
    image[at..at + bytes.len()].copy_from_slice(bytes);
    MemBlockDevice::from_bytes(&image)
}

fn assert_mount_rejected(dev: MemBlockDevice) {
    match Ext2Filesystem::new(dev, || 0) {
        Err(akuma_vfs::FsError::Corrupt) => {}
        Err(e) => panic!("expected FsError::Corrupt, got {e:?}"),
        Ok(_) => panic!("expected FsError::Corrupt, got a successful mount"),
    }
}

/// `inode_size < 128` made `read_inode` blit a whole `Inode` out of a smaller
/// heap buffer — the one §2.3 finding that was memory-unsafe, not just a panic.
#[test]
fn mount_rejects_inode_size_below_the_inode_struct() {
    assert_mount_rejected(fixture_with_sb_field(88, &64u16.to_le_bytes()));
}

#[test]
fn mount_rejects_zero_inode_size() {
    assert_mount_rejected(fixture_with_sb_field(88, &0u16.to_le_bytes()));
}

/// `block_group_count = (...)/blocks_per_group` divided by a disk-supplied 0.
#[test]
fn mount_rejects_zero_blocks_per_group() {
    assert_mount_rejected(fixture_with_sb_field(32, &0u32.to_le_bytes()));
}

/// `inode_idx / inodes_per_group` on every inode read divided by 0 — this one
/// panicked past mount, at the first lookup, before the §2.3 sweep.
#[test]
fn mount_rejects_zero_inodes_per_group() {
    assert_mount_rejected(fixture_with_sb_field(40, &0u32.to_le_bytes()));
}

/// `1024usize << block_size_log` with a disk-supplied log: debug builds
/// panicked on the shift overflow, release builds wrapped into a garbage
/// block size that mistook every later byte on the disk.
#[test]
fn mount_rejects_block_size_log_over_the_spec_range() {
    assert_mount_rejected(fixture_with_sb_field(24, &7u32.to_le_bytes()));
    assert_mount_rejected(fixture_with_sb_field(24, &31u32.to_le_bytes()));
    assert_mount_rejected(fixture_with_sb_field(24, &u32::MAX.to_le_bytes()));
}

/// `(total_blocks - first_data_block)` underflowed when a corrupt image put
/// the first data block past the end of the device.
#[test]
fn mount_rejects_first_data_block_past_total_blocks() {
    assert_mount_rejected(fixture_with_sb_field(20, &u32::MAX.to_le_bytes()));
}

/// `apk`'s cache write, end to end at the fs layer: create-or-replace a file
/// **nested three levels deep**, then rename it within the same directory —
/// the atomic `.tmp.<pid>` + rename shape. Found 2026-09-04: the write
/// half succeeded to root-level paths but `rename` returned NotFound for a
/// nested old path, and `sys_close`'s discarded `write_file` result hid the
/// other half.
#[test]
fn apk_cache_write_and_rename_nested() {
    let fs = mount_empty();
    // `mkdisk.sh` pre-creates these on the amd64 image; the ext2 write path
    // does not create parents, matching `open(2)`.
    fs.create_dir("/var").unwrap();
    fs.create_dir("/var/cache").unwrap();
    fs.create_dir("/var/cache/apk").unwrap();
    let body: alloc::vec::Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    fs.write_file("/var/cache/apk/APKINDEX.cf3ffe0d.tar.gz.tmp.1", &body)
        .unwrap();
    fs.rename(
        "/var/cache/apk/APKINDEX.cf3ffe0d.tar.gz.tmp.1",
        "/var/cache/apk/APKINDEX.cf3ffe0d.tar.gz",
    )
    .unwrap();
    assert!(fs.read_file("/var/cache/apk/APKINDEX.cf3ffe0d.tar.gz.tmp.1").is_err());
    let back = fs.read_file("/var/cache/apk/APKINDEX.cf3ffe0d.tar.gz").unwrap();
    assert_eq!(back, body);
}

/// The REAL amd64 guest image (mkfs.ext2 -b 1024 + debugfs), tmp-file write
/// then rename, exactly as apk's cache update does. Requires the image at
/// /tmp/amd64-root-for-test.img (copied there by the debugging session);
/// skipped silently when it is absent so CI does not depend on it.
#[test]
fn guest_image_cache_write_and_rename() {
    extern crate std;
    let path = "/tmp/amd64-root-for-test.img";
    let Ok(bytes) = std::fs::read(path) else { return };
    let dev = MemBlockDevice::from_bytes(&bytes);
    let fs = Ext2Filesystem::new(dev, || 0).unwrap();
    let body: alloc::vec::Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    fs.write_file("/var/cache/apk/APKINDEX.cf3ffe0d.tar.gz.tmp.1", &body).unwrap();
    fs.rename(
        "/var/cache/apk/APKINDEX.cf3ffe0d.tar.gz.tmp.1",
        "/var/cache/apk/APKINDEX.cf3ffe0d.tar.gz",
    ).unwrap();
    let back = fs.read_file("/var/cache/apk/APKINDEX.cf3ffe0d.tar.gz").unwrap();
    assert_eq!(back, body);
}

/// `ftruncate`'s extend half must actually extend: allocate zero blocks, zero
/// the tail of the old EOF's block, and update the inode size.
///
/// The silent no-op this pins shut answered `Ok(())` for every extension ("not
/// implemented; bun only shrinks") — and then parity-db called `set_len`
/// before mapping its files, cargo's flock cache grew, and every caller got
/// success plus a zero-byte file. Read back through a *fresh* descriptor so
/// the assertion goes through the inode on disk, not a stale cache.
#[test]
fn ftruncate_extends_with_zero_blocks() {
    let fs = mount_empty();
    fs.write_file("/grow", b"head").unwrap();

    fs.truncate("/grow", 10 * 1024).unwrap();

    // The inode size moved, visible to metadata.
    let meta = fs.metadata("/grow").unwrap();
    assert_eq!(meta.size, 10 * 1024, "the extend must be real, not accepted-and-dropped");

    // The old bytes survive, the gap is zeros, the tail is zeros.
    let mut buf = vec![0u8; 10 * 1024];
    let n = fs.read_at("/grow", 0, &mut buf).unwrap();
    assert_eq!(n, 10 * 1024);
    assert_eq!(&buf[..4], b"head");
    assert!(buf[4..10 * 1024].iter().all(|&b| b == 0), "extended bytes must read as zeros");

    // Shrink still shrinks, and frees what it drops.
    fs.truncate("/grow", 4).unwrap();
    let meta = fs.metadata("/grow").unwrap();
    assert_eq!(meta.size, 4);
    let mut small = [0u8; 8];
    let n = fs.read_at("/grow", 0, &mut small).unwrap();
    assert_eq!(n, 4);
    assert_eq!(&small[..4], b"head");

    // Extend again past the shrink: the same zero-block rule, from a hole.
    fs.truncate("/grow", 9 * 1024 + 512).unwrap();
    let mut buf = vec![0u8; 9 * 1024 + 512];
    let n = fs.read_at("/grow", 0, &mut buf).unwrap();
    assert_eq!(n, 9 * 1024 + 512);
    assert_eq!(&buf[..4], b"head");
    assert!(buf[4..].iter().all(|&b| b == 0));
}

/// `ftruncate` shrink must free the blocks it drops and must not let them come
/// back: shrink-then-extend reads **zeros** past the shrink point.
///
/// Until 2026-10-03 `truncate` only moved `i_size` on a shrink. The blocks
/// stayed allocated and referenced (so `e2fsck` reported `i_size is 77824,
/// should be 98304` on a file a mapped-file stress had shrunk), and a later
/// extend reused them without zeroing — the old bytes reappeared where POSIX
/// promises zeros. The earlier test of this name's neighbour asserted only the
/// size, which is why it passed.
#[test]
fn ftruncate_shrink_frees_blocks_and_reextend_reads_zeros() {
    let fs = mount_empty();
    let bs = fs.stats().unwrap().block_size as usize;
    // Large enough to need the singly-indirect block (12 direct blocks).
    let len = bs * 40;
    fs.write_file("/shrink", &vec![0xAA; len]).unwrap();
    let free_full = fs.stats().unwrap().free_blocks;

    fs.truncate("/shrink", 100).unwrap();
    let free_small = fs.stats().unwrap().free_blocks;
    // 40 data blocks + 1 indirect block, less the one block that still holds
    // the 100 surviving bytes.
    assert_eq!(free_small - free_full, 40, "shrink must give back the dropped blocks (and the indirect block)");

    fs.truncate("/shrink", len as u64).unwrap();
    let mut buf = vec![0xFFu8; len];
    assert_eq!(fs.read_at("/shrink", 0, &mut buf).unwrap(), len);
    assert!(buf[..100].iter().all(|&b| b == 0xAA), "surviving bytes must survive");
    assert!(
        buf[100..].iter().all(|&b| b == 0),
        "bytes past a shrink must read as zeros after the file grows back"
    );

    // Shrink to a block boundary, and to zero.
    fs.truncate("/shrink", (bs * 3) as u64).unwrap();
    fs.truncate("/shrink", 0).unwrap();
    assert_eq!(fs.stats().unwrap().free_blocks, free_full + 41, "an emptied file holds no blocks");
}

/// Shrink through every level of the block map: direct, singly indirect and
/// doubly indirect, to a point inside each, then to zero. The free count must
/// return to its starting value exactly (no leaked pointer block, no block
/// freed twice) and the surviving prefix must be intact.
#[test]
fn ftruncate_shrink_walks_every_indirection_level() {
    let fs = mount_empty();
    let bs = fs.stats().unwrap().block_size as usize;
    let ppb = bs / 4;
    let baseline = fs.stats().unwrap().free_blocks;
    let len = bs * (12 + ppb + 3 * ppb / 2); // well into the doubly-indirect range
    let pattern: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    fs.write_file("/deep", &pattern).unwrap();

    for &cut in &[len - bs / 2, bs * (12 + ppb + ppb / 2), bs * (12 + ppb + 1) + 7, bs * (12 + ppb), bs * 12 + 1, bs * 5 + 3, 0usize] {
        fs.truncate("/deep", cut as u64).unwrap();
        assert_eq!(fs.metadata("/deep").unwrap().size, cut as u64);
        let mut back = vec![0u8; cut];
        assert_eq!(fs.read_at("/deep", 0, &mut back).unwrap(), cut);
        assert_eq!(back, pattern[..cut], "prefix intact after shrinking to {cut}");
    }
    assert_eq!(fs.stats().unwrap().free_blocks, baseline, "shrinking to zero returns every block, pointer blocks included");

    // And the file is still usable, with no stale bytes, afterwards.
    fs.truncate("/deep", len as u64).unwrap();
    let mut back = vec![0xFFu8; len];
    fs.read_at("/deep", 0, &mut back).unwrap();
    assert!(back.iter().all(|&b| b == 0));
    // No `remove_file` + free-count check here: the inode pin table is a
    // process-global keyed by inode number, so a parallel test that pins the
    // same number defers this unlink's free and the count moves for a reason
    // that is not what this test is about.
}

/// The stress plan from `docs/archive/AKUMA_AMD64_EXT2_CROSS_FILE_CORRUPTION.md`
/// §8, as one host test: every round rewrites a config (`O_TRUNC` frees its
/// blocks and allocates new ones), truncates a log, replaces a 100 KiB file by
/// writing a new one and renaming it over the old (`wget` + `mv`), then has two
/// threads append to the log while a third rewrites the config again — the
/// sequence that, on the trashcan, left the config holding the log's text.
///
/// Two oracles. **Content**: every file reads back as exactly what was last
/// written to it, so a block handed to two inodes shows up as one file's bytes
/// inside another. **`e2fsck -fn`** on the device image afterwards: blocks
/// claimed twice, referenced-but-free blocks and bitmap differences are its
/// bread and butter. The second is skipped (with a note) on a host without
/// e2fsprogs.
#[test]
fn rewrite_truncate_rename_and_concurrent_append_keep_files_apart() {
    extern crate std;
    let dev = RecordingDevice::from_fixture("manyinodes.ext2");
    // A real clock: with `|| 0` every freed inode's `dtime` is 0, which
    // `e2fsck` reports as "deleted inode has zero dtime" — the fixture's
    // fault, not the driver's (`remove_dir_zeroes_the_directorys_link_count`).
    let clock = || 1_700_000_000_000_000;
    let fs = Ext2Filesystem::new(&dev, clock).unwrap();
    fs.create_dir("/tmp").unwrap();

    let cfg = |seed: u32| -> Vec<u8> {
        let mut v = Vec::new();
        while v.len() < 2300 {
            v.extend_from_slice(alloc::format!("[cfg {seed}] key{} = \"value\"\n", v.len()).as_bytes());
        }
        v
    };
    let big = |seed: u32| -> Vec<u8> { (0..100 * 1024u32).map(|i| (i.wrapping_mul(31) ^ seed) as u8).collect() };

    for round in 0..30u32 {
        fs.write_file("/tmp/cfg", &cfg(round)).unwrap();
        if fs.exists("/tmp/log") {
            fs.truncate("/tmp/log", 0).unwrap();
        }
        fs.write_file("/tmp/big.new", &big(round)).unwrap();
        fs.rename("/tmp/big.new", "/tmp/big").unwrap();

        let cfg2 = cfg(round + 1000);
        std::thread::scope(|s| {
            for t in 0..2u32 {
                let fs = &fs;
                s.spawn(move || {
                    for i in 0..60 {
                        let line = alloc::format!("r{round} t{t} i{i:02} trace trace trace trace\n");
                        fs.append("/tmp/log", line.as_bytes()).unwrap();
                    }
                });
            }
            let fs = &fs;
            let cfg2 = &cfg2;
            s.spawn(move || fs.write_file("/tmp/cfg", cfg2).unwrap());
        });

        assert_eq!(fs.read_file("/tmp/cfg").unwrap(), cfg2, "round {round}: the config's data was replaced");
        assert_eq!(fs.read_file("/tmp/big").unwrap(), big(round), "round {round}: the big file's data was replaced");
        let log = fs.read_file("/tmp/log").unwrap();
        let text = core::str::from_utf8(&log).expect("the log holds only its own lines");
        let prefix = alloc::format!("r{round} t");
        assert_eq!(text.lines().count(), 120, "round {round}: appends lost or log cross-linked");
        assert!(text.lines().all(|l| l.starts_with(&prefix) && l.ends_with("trace")), "round {round}: a torn or foreign line");
    }

    // Fresh mount over the same bytes: what reached the device agrees.
    let fs2 = Ext2Filesystem::new(&dev, clock).unwrap();
    assert_eq!(fs2.read_file("/tmp/cfg").unwrap(), cfg(29 + 1000));

    let e2fsck = ["/opt/homebrew/opt/e2fsprogs/sbin/e2fsck", "/usr/local/opt/e2fsprogs/sbin/e2fsck", "/sbin/e2fsck", "/usr/sbin/e2fsck"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists());
    let Some(e2fsck) = e2fsck else {
        std::eprintln!("note: e2fsck not found; skipping the on-disk consistency oracle");
        return;
    };
    let img = std::env::temp_dir().join(alloc::format!("akuma-ext2-stress-{}.img", std::process::id()));
    std::fs::write(&img, &*dev.inner.lock()).unwrap();
    let out = std::process::Command::new(e2fsck).arg("-fn").arg(&img).output().unwrap();
    let _ = std::fs::remove_file(&img);
    assert!(
        out.status.success(),
        "e2fsck -fn found problems:\n{}{}",
        alloc::string::String::from_utf8_lossy(&out.stdout),
        alloc::string::String::from_utf8_lossy(&out.stderr)
    );
}

/// The corruption the trashcan produced on 2026-10-05: an 8.5 MB kernel image
/// pushed with `cat > file` over an older copy read back correctly, and a few
/// minutes and some unrelated file activity later five 4 KiB blocks of its
/// **double-indirect** range held stale contents — the previous copy's bytes,
/// at the previous copy's offsets. The shape here: a file deep into the
/// double-indirect range, overwritten by `truncate(0)` and unaligned
/// sequential writes, with a block cache small enough to evict, then checked
/// through the same mount *and* a fresh mount of the device (the disk's truth).
#[test]
fn overwrite_of_a_double_indirect_file_survives_eviction() {
    extern crate std;
    let dev = RecordingDevice::from_fixture("manyinodes.ext2");
    let clock = || 1_700_000_000_000_000;
    // 32 KiB of cache on a 1 KiB-block image: thousands of evictions.
    let fs = Ext2Filesystem::new_with_cache_cap(&dev, clock, Some(32 * 1024)).unwrap();
    // 1 KiB blocks: 12 direct + 256 indirect = 268 KiB before double-indirect.
    const LEN: usize = 900 * 1024;
    let image = |seed: u32| -> Vec<u8> {
        (0..LEN as u32).map(|i| (i.wrapping_mul(2_654_435_761).rotate_left(seed) >> 13) as u8).collect()
    };
    let write_chunked = |path: &str, data: &[u8]| {
        let mut pos = 0;
        while pos < data.len() {
            let n = (data.len() - pos).min(7_000);
            assert_eq!(fs.write_at(path, pos, &data[pos..pos + n]).unwrap(), n);
            pos += n;
        }
    };
    fs.create_dir("/root").unwrap();
    write_chunked("/root/k.new", &image(1));
    for round in 2..6u32 {
        // `cat > /root/k.new`: O_TRUNC, then the stream.
        fs.truncate("/root/k.new", 0).unwrap();
        let want = image(round);
        write_chunked("/root/k.new", &want);
        // Unrelated churn in between, as on the box: small files written,
        // truncated, appended to and removed.
        for i in 0..20 {
            let p = alloc::format!("/root/small{}", i % 4);
            fs.write_file(&p, &vec![i as u8; 3000 + i * 50]).unwrap();
            fs.truncate(&p, 0).unwrap();
            fs.append(&p, b"line\n").unwrap();
        }
        let got = fs.read_file("/root/k.new").unwrap();
        let first_bad = got.iter().zip(&want).position(|(a, b)| a != b);
        assert!(got.len() == want.len() && first_bad.is_none(),
            "round {round}: cached view differs at {first_bad:?} (len {} vs {})", got.len(), want.len());
        let fresh = Ext2Filesystem::new(&dev, clock).unwrap();
        let disk = fresh.read_file("/root/k.new").unwrap();
        let first_bad = disk.iter().zip(&want).position(|(a, b)| a != b);
        assert!(disk.len() == want.len() && first_bad.is_none(),
            "round {round}: on-disk view differs at {first_bad:?}");
    }
}


// ============================================================================
// The trashcan's 2026-10-05 stale-block corruption (handoff:
// docs/runbooks/debug-ext2-corruption.md)
// ============================================================================

/// Device wrapper that yields inside `read_bytes`, widening the `with_block`
/// miss window. On the trashcan a read is a USB mass-storage round-trip —
/// milliseconds — while every cache operation is nanoseconds, so the gap
/// between "miss, start the device read" and "insert what came back" is
/// proportionally far wider there than any host test makes it. A yield per
/// read is the cheap host model of that asymmetry.
struct UsbLatencyDevice {
    inner: RecordingDevice,
}

impl UsbLatencyDevice {
    fn from_fixture(name: &str) -> Self {
        Self { inner: RecordingDevice::from_fixture(name) }
    }
}

impl BlockDevice for UsbLatencyDevice {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        extern crate std;
        std::thread::yield_now();
        self.inner.read_bytes(offset, buf)
    }

    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
        self.inner.write_bytes(offset, data)
    }
}

/// Mounted through `&UsbLatencyDevice` so the device can also back the fresh
/// mount of the persistence oracle.
impl BlockDevice for &UsbLatencyDevice {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        (**self).read_bytes(offset, buf)
    }

    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
        (**self).write_bytes(offset, data)
    }
}

/// A minimal independent ext2 walker — the host's `e2fsck -fn` replacement.
///
/// Parses superblock/BGD/inode bytes straight from the device (not through the
/// driver's own structures, so a driver-side record bug cannot hide itself) and
/// checks the two corruption signatures the trashcan produced:
///
/// 1. **no block claimed by two inodes** (cross-file corruption), and
/// 2. **every claimed block is set in its group's bitmap** (a stale free or a
///    lost bitmap update leaves a block claimed-but-free, which is exactly what
///    hands it to the next writer).
///
/// Data blocks are collected up to `i_size`; indirect and double-indirect
/// pointer blocks themselves are claimed too. `e2fsck`-style free-count
/// reconciliation is out of scope — the two checks above are the incident's.
fn fsck_style_check(dev: &UsbLatencyDevice) -> Result<(), alloc::string::String> {
    extern crate std;

    let le32 = |b: &[u8], o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let le16 = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);

    let raw = dev.inner.inner.lock();
    let read_block = |bn: u32, bs: usize| -> Vec<u8> {
        raw[bn as usize * bs..(bn as usize + 1) * bs].to_vec()
    };

    let sb = &raw[1024..];
    let inodes_count = le32(sb, 0) as usize;
    let first_data_block = le32(sb, 20) as usize;
    let log_block_size = le32(sb, 24) as usize;
    let block_size = 1024 << log_block_size;
    let blocks_per_group = le32(sb, 32) as usize;
    let inodes_per_group = le32(sb, 40) as usize;
    let inode_size = le16(sb, 88) as usize;
    let blocks_count = le32(sb, 4) as usize;
    let _group_count = (blocks_count - first_data_block + blocks_per_group - 1) / blocks_per_group;

    let bgd_table = (first_data_block + 1) * block_size;
    let bgd = |g: usize| -> (usize, usize, usize) {
        let at = bgd_table + g * 32;
        (
            le32(&raw, at) as usize,
            le32(&raw, at + 4) as usize,
            le32(&raw, at + 8) as usize,
        )
    };

    // inode number -> every data/metadata block it claims, in claim order.
    let mut claims: alloc::collections::BTreeMap<u32, Vec<(u32, u32)>> = Default::default();
    let ppb = block_size / 4;

    let note = |claims: &mut alloc::collections::BTreeMap<u32, Vec<(u32, u32)>>,
                    ino: u32,
                    block: u32,
                    logical: u32| {
        claims.entry(block).or_default().push((ino, logical));
    };

    for ino in 1..=inodes_count {
        let g = (ino - 1) / inodes_per_group;
        let idx = (ino - 1) % inodes_per_group;
        let (_, _, inode_table) = bgd(g);
        let at = inode_table * block_size + idx * inode_size;
        let mode = le16(&raw, at);
        let fmt = mode & 0xF000;
        if fmt == 0 {
            continue; // free inode
        }
        let size = le32(&raw, at + 4) as u64;
        let ptr_at = at + 40;
        let ptr = |i: usize| le32(&raw, ptr_at + i * 4);

        let ino_id = ino as u32;
        // Direct.
        for i in 0..12.min(size.div_ceil(block_size as u64) as usize) {
            if ptr(i) != 0 {
                note(&mut claims, ino_id, ptr(i), i as u32);
            }
        }
        let walk_indirect = |bn: u32, base: u32, count: u32, claims: &mut _,
                                 read_block: &dyn Fn(u32, usize) -> Vec<u8>| {
            if bn == 0 {
                return;
            }
            note(claims, ino_id, bn, u32::MAX); // the pointer block itself
            let blk = read_block(bn, block_size);
            for j in 0..count as usize {
                let b = le32(&blk, j * 4);
                if b != 0 {
                    note(claims, ino_id, b, base + j as u32);
                }
            }
        };
        let data_blocks = size.div_ceil(block_size as u64) as u32;
        let single = data_blocks.saturating_sub(12).min(ppb as u32);
        walk_indirect(ptr(12), 12, single, &mut claims, &read_block);
        // Double.
        let dstart = 12 + ppb as u32;
        let dbl_count = data_blocks.saturating_sub(dstart).min((ppb * ppb) as u32);
        if ptr(13) != 0 {
            note(&mut claims, ino_id, ptr(13), u32::MAX);
            let dbl = read_block(ptr(13), block_size);
            let mut remaining = dbl_count;
            for i in 0..ppb {
                if remaining == 0 {
                    break;
                }
                let ib = le32(&dbl, i * 4);
                let take = remaining.min(ppb as u32);
                walk_indirect(ib, dstart + i as u32 * ppb as u32, take, &mut claims, &read_block);
                remaining -= take;
            }
        }
        if ptr(14) != 0 {
            return Err(alloc::format!("inode {ino}: triple-indirect present — not expected here"));
        }
    }

    // Signature 1: a block claimed twice.
    for (block, who) in &claims {
        if who.len() > 1 {
            let names: alloc::vec::Vec<alloc::string::String> =
                who.iter().map(|(i, l)| alloc::format!("inode {i} lb {l}")).collect();
            return Err(alloc::format!(
                "block {block} claimed by: {}",
                names.join(", ")
            ));
        }
    }

    // Signature 2: a claimed block not set in its bitmap.
    let mut bitmap_cache: alloc::collections::BTreeMap<u32, Vec<u8>> = Default::default();
    let bit_set = |bitmap_cache: &mut alloc::collections::BTreeMap<u32, Vec<u8>>,
                       block: u32|
     -> bool {
        let g = (block - first_data_block as u32) / blocks_per_group as u32;
        let (bitmap_block, _, _) = {
            let (bb, ib, it) = bgd(g as usize);
            (bb as u32, ib as u32, it as u32)
        };
        let data = bitmap_cache
            .entry(bitmap_block)
            .or_insert_with(|| read_block(bitmap_block, block_size));
        let bit = (block - first_data_block as u32) % blocks_per_group as u32;
        data[(bit / 8) as usize] & (1 << (bit % 8)) != 0
    };
    for &block in claims.keys() {
        if !bit_set(&mut bitmap_cache, block) {
            let who = &claims[&block];
            return Err(alloc::format!(
                "block {block} claimed by inode {} lb {} but clear in its group {} bitmap",
                who[0].0, who[0].1, (block - first_data_block as u32) / blocks_per_group as u32,
            ));
        }
    }
    Ok(())
}

/// §9 of `AKUMA_AMD64_EXT2_CROSS_FILE_CORRUPTION.md`, with the two dimensions
/// the earlier repros were missing (the handoff names both): **inode pins
/// driving deferred frees**, and **concurrency** — the trashcan is SMP, and the
/// push, the deploy renames, the appends, the truncations, the probes' reads
/// and the deferred-free drains all ran at once.
///
/// The shape, per the incident record: a kernel image pushed with `cat >`
/// (O_TRUNC over a similar-sized older copy, then a chunked stream that runs
/// deep into the double-indirect range); a deploy `mv` over the previous image
/// (rename-over frees ~2 100 blocks inline); probe binaries renamed over while
/// still mapped (pin held → the old inode's free is deferred); rounds of
/// `: > B` plus concurrent `>>` appends; reads interleaved throughout. The
/// cache is capped small so the eviction the box gets from streaming GBs
/// through a real build happens here too.
///
/// Oracles: the same mount's view after each round, a fresh mount's view (the
/// disk's truth), and `e2fsck -fn` on the final image.
#[test]
fn pinned_unlink_deferred_free_and_concurrent_rewrite_keep_blocks_apart() {
    extern crate std;
    use std::sync::Barrier;

    let _serial = pin_test_serial();
    let dev = UsbLatencyDevice::from_fixture("manyinodes.ext2");
    let clock = || 1_700_000_000_000_000;

    // Both cache regimes:
    // - 64 KiB: thousands of evictions, the streaming-a-build-through-the-cache
    //   regime;
    // - 4 MiB: the whole fixture stays resident — the box's regime, where
    //   nothing evicts and the only way a dirty copy dies unflushed is the
    //   invalidate-on-free rule. If a stale free exists, THIS arm is where it
    //   shows.
    let regimes: [Option<usize>; 2] = [Some(64 * 1024), Some(4 * 1024 * 1024)];
    let rounds: u32 = std::env::var("E2_STRESS_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(6);
    let appenders: u32 = 2;

    const K_LEN: usize = 896 * 1024; // double-indirect entries to ~628 — the incident's range
    let image = |seed: u32| -> Vec<u8> {
        (0..K_LEN as u32).map(|i| (i.wrapping_mul(2_654_435_761).rotate_left(seed) >> 13) as u8).collect()
    };
    let probe = |seed: u32| -> Vec<u8> {
        (0..7 * 1024 + seed as usize * 13).map(|i| (i.wrapping_mul(31) ^ seed as usize) as u8).collect()
    };

    for regime in regimes {
        let regime_name = if regime.unwrap() <= 64 * 1024 { "evicting" } else { "resident" };
        let fs = alloc::sync::Arc::new(
            Ext2Filesystem::new_with_cache_cap(&dev, clock, regime).unwrap(),
        );
        if !fs.exists("/root") {
            fs.create_dir("/root").unwrap();
        }
        // `touch` the append/truncate targets so round 1 starts settled.
        fs.write_file("/root/log", b"").unwrap();
        fs.write_file("/root/B", b"").unwrap();
        let log_inode = fs.resolve_inode("/root/log").unwrap();

        for round in 1..=rounds {
            let want = image(round);
            // pusher, deployer, prober, deferrer, appenders ×2, truncator, main.
            let barrier1 = alloc::sync::Arc::new(Barrier::new(4 + appenders as usize + 2));
            let barrier2 = barrier1.clone();

            std::thread::scope(|s| {
                // ── the push: `cat > /root/kernel` (O_TRUNC over the old copy). ──
                let pusher = {
                    let fs = fs.clone();
                    let b1 = barrier1.clone();
                    let b2 = barrier2.clone();
                    let want = want.clone();
                    s.spawn(move || {
                        // `cat > /root/kernel`: create if missing, else O_TRUNC.
                        fs.write_file("/root/kernel", b"").unwrap();
                        b1.wait();
                        let mut pos = 0;
                        while pos < want.len() {
                            let n = (want.len() - pos).min(15 * 1024 + 512);
                            assert_eq!(fs.write_at("/root/kernel", pos, &want[pos..pos + n]).unwrap(), n);
                            pos += n;
                            // The push arrives over ssh: the state lock is free
                            // in the gaps between chunks, and everyone else
                            // runs there.
                            std::thread::yield_now();
                        }
                        b2.wait();
                    })
                };

                // ── the deploy: a fresh image written, `mv` over the previous. ──
                let deployer = {
                    let fs = fs.clone();
                    let b1 = barrier1.clone();
                    let b2 = barrier2.clone();
                    s.spawn(move || {
                        b1.wait();
                        let fresh = alloc::format!("/root/kernel.new{round}");
                        let data = image(round + 100);
                        let mut pos = 0;
                        while pos < data.len() {
                            let n = (data.len() - pos).min(9 * 1024);
                            assert_eq!(fs.write_at(&fresh, pos, &data[pos..pos + n]).unwrap(), n);
                            pos += n;
                        }
                        fs.rename(&fresh, "/root/kernel.prev").unwrap();
                        b2.wait();
                    })
                };

                // ── the probes: renamed over while still mapped. ──
                let prober = {
                    let fs = fs.clone();
                    let b1 = barrier1.clone();
                    let b2 = barrier2.clone();
                    s.spawn(move || {
                        b1.wait();
                        for pgen in 0..12u32 {
                            let name = alloc::format!("/root/probe{}", pgen % 3);
                            let new = alloc::format!("{name}.new");
                            let data = probe(pgen);
                            fs.write_file(&new, &data).unwrap();
                            let pin = akuma_primitives::InodePin::new(fs.resolve_inode(&new).unwrap());
                            fs.rename(&new, &name).unwrap();
                            // Allocating in between runs the drain; the pin
                            // keeps the queued inode's blocks owned meanwhile.
                            let _ = fs.write_file(&alloc::format!("/root/churn{pgen}"), b"churn");
                            std::thread::yield_now();
                            drop(pin); // mapping gone — the next allocation reclaims it
                            let _ = fs.write_file("/root/churn", b"c");
                        }
                        b2.wait();
                    })
                };

                // ── the mapped set: several pins live at once across many
                // drains. `cargo`'s shape: dozens of mapped files while the
                // build churns thousands of allocations past the deferral
                // list, every drain SKIPPING the live entries.
                let deferrer = {
                    let fs = fs.clone();
                    let b1 = barrier1.clone();
                    let b2 = barrier2.clone();
                    s.spawn(move || {
                        b1.wait();
                        for dgen in 0..6u32 {
                            let mut pins = alloc::vec::Vec::new();
                            for k in 0..4u32 {
                                let name = alloc::format!("/root/map{dgen}{k}");
                                fs.write_file(&name, &vec![k as u8; 3000 + k as usize * 111]).unwrap();
                                pins.push(akuma_primitives::InodePin::new(fs.resolve_inode(&name).unwrap()));
                            }
                            for k in 0..4u32 {
                                let name = alloc::format!("/root/map{dgen}{k}");
                                fs.remove_file(&name).unwrap(); // all queued, all pinned
                            }
                            for c in 0..30 {
                                let _ = fs.write_file(&alloc::format!("/root/mchurn{dgen}{c}"), b"m");
                                fs.remove_file(&alloc::format!("/root/mchurn{dgen}{c}")).unwrap();
                            }
                            drop(pins); // all reclaimable at the next allocation
                            let _ = fs.write_file("/root/mchurn", b"m");
                        }
                        b2.wait();
                    })
                };

                // ── the two-appender repro, each append wrapped in the fd's
                // pin (open → append → close, which is what `echo >>` is). ──
                let mut append_handles = alloc::vec::Vec::new();
                for t in 0..appenders {
                    let fs = fs.clone();
                    let b1 = barrier1.clone();
                    let b2 = barrier2.clone();
                    append_handles.push(s.spawn(move || {
                        b1.wait();
                        for i in 0..80 {
                            // open(2) takes the fd pin; drop = close(2).
                            let fd_pin = akuma_primitives::InodePin::new(log_inode);
                            let line = alloc::format!("r{round} t{t} i{i:03} trace trace trace trace\n");
                            fs.append("/root/log", line.as_bytes()).unwrap();
                            drop(fd_pin);
                        }
                        b2.wait();
                    }));
                }
                let truncator = {
                    let fs = fs.clone();
                    let b1 = barrier1.clone();
                    let b2 = barrier2.clone();
                    s.spawn(move || {
                        b1.wait();
                        for i in 0..40 {
                            // `: > B` while the appenders are mid-`>>` on the
                            // same file: the blocks cycle free→allocated with
                            // the scan hint pulling reallocation to the same
                            // numbers, under fd-pin churn.
                            let b_pin = akuma_primitives::InodePin::new(log_inode);
                            fs.truncate("/root/log", 0).unwrap();
                            fs.append("/root/log", alloc::format!("B r{round} i{i:02}\n").as_bytes()).unwrap();
                            drop(b_pin);
                        }
                        b2.wait();
                    })
                };

                // ── readers, interleaved the whole round: md5s, stat, listings. ──
                let reader = {
                    let fs = fs.clone();
                    s.spawn(move || {
                        for _ in 0..400 {
                            let _ = fs.read_dir("/root");
                            let _ = fs.metadata("/root/kernel");
                            let _ = fs.read_at("/root/log", 0, &mut [0u8; 512]);
                            std::thread::yield_now();
                        }
                    })
                };

                barrier1.wait();
                barrier2.wait();
                pusher.join().unwrap();
                deployer.join().unwrap();
                prober.join().unwrap();
                deferrer.join().unwrap();
                for h in append_handles {
                    h.join().unwrap();
                }
                truncator.join().unwrap();
                reader.join().unwrap();
            });

            // ── round oracle 1: the same mount serves exactly what was pushed. ──
            let got = fs.read_file("/root/kernel").unwrap();
            let first_bad = got.iter().zip(&want).position(|(a, b)| a != b);
            assert!(
                got.len() == want.len() && first_bad.is_none(),
                "{regime_name} round {round}: cached view of the kernel differs at {first_bad:?} (len {} vs {})",
                got.len(), want.len(),
            );
            // ── round oracle 2: the disk's truth, through a fresh mount. ──
            let fresh = Ext2Filesystem::new_with_cache_cap(&dev, clock, regime).unwrap();
            let disk = fresh.read_file("/root/kernel").unwrap();
            let first_bad = disk.iter().zip(&want).position(|(a, b)| a != b);
            assert!(
                disk.len() == want.len() && first_bad.is_none(),
                "{regime_name} round {round}: on-disk view of the kernel differs at {first_bad:?} (len {} vs {})",
                disk.len(), want.len(),
            );
            // ── round oracle 3: no block claimed twice; every claim bitmap-set. ──
            if let Err(problem) = fsck_style_check(&dev) {
                panic!("{regime_name} round {round}: {problem}");
            }
        }
    }
}

/// `sync()` is a media promise, not a cache-internal one: after it, the
/// device's own barrier must have been asked for. The 2026-10-05 reboot lost
/// acknowledged writes because nothing below the write-back cache ever
/// reached the media barrier — `sync(2)` was not even dispatched on amd64,
/// and the SCSI `SYNCHRONIZE CACHE` that the USB drive's volatile cache needs
/// did not exist (corruption doc §11). `Filesystem::sync` now ends at
/// `BlockDevice::flush`.
#[test]
fn sync_reaches_the_device_barrier_and_persists() {
    extern crate std;
    let dev = RecordingDevice::from_fixture("test.ext2");
    let fs = Ext2Filesystem::new(&dev, || 0).unwrap();
    fs.create_dir("/root").unwrap();

    let before = dev.flushes.load(AtomicOrdering::Relaxed);
    fs.write_file("/root/durable", b"must survive a cold read").unwrap();
    assert_eq!(
        dev.flushes.load(AtomicOrdering::Relaxed),
        before,
        "plain writes push their own blocks but must not claim a media barrier"
    );

    use akuma_vfs::Filesystem as _;
    fs.sync().unwrap();
    assert_eq!(
        dev.flushes.load(AtomicOrdering::Relaxed),
        before + 1,
        "sync() must end at the device's media barrier"
    );

    // And the promise is real: a fresh mount over the same bytes reads it.
    let fresh = Ext2Filesystem::new(&dev, || 0).unwrap();
    assert_eq!(fresh.read_file("/root/durable").unwrap(), b"must survive a cold read");
}
