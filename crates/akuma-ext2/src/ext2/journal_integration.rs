//! Host-only experiment: run existing ext2 mutations against a staged ext3
//! device and commit each successful operation with the journal writer. This
//! is intentionally not a production writable mount: error rollback,
//! transaction isolation and device durability are not yet wired into VFS.

use super::*;
use super::journal::{staging::WriteSet, writer::{self, JournalIo}};
use alloc::sync::Arc;
use std::process::Command;

const BS: usize = 1024;

struct Inner {
    disk: Vec<u8>,
    staged: WriteSet,
    journal: Vec<u32>,
}
struct StagedDevice(Arc<Spinlock<Inner>>);

impl BlockDevice for StagedDevice {
    fn read_bytes(&self, offset: u64, out: &mut [u8]) -> Result<(), ()> {
        let state = self.0.lock();
        let start = usize::try_from(offset).map_err(|_| ())?;
        let end = start.checked_add(out.len()).ok_or(())?;
        out.copy_from_slice(state.disk.get(start..end).ok_or(())?);
        if out.is_empty() { return Ok(()); }
        for block in start / BS..=(end - 1) / BS {
            if let Some(data) = state.staged.get(block as u32) {
                let lo = start.max(block * BS);
                let hi = end.min((block + 1) * BS);
                out[lo - start..hi - start].copy_from_slice(&data[lo - block * BS..hi - block * BS]);
            }
        }
        Ok(())
    }
    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
        let mut state = self.0.lock();
        let Inner { disk, staged, .. } = &mut *state;
        staged.stage(offset, data, |block| {
            let start = block as usize * BS;
            Ok(disk.get(start..start + BS).ok_or(FsError::IoError)?.to_vec())
        }).map_err(|_| ())
    }
}

impl JournalIo for Inner {
    fn read_journal(&mut self, block: u32) -> Result<Vec<u8>, FsError> {
        let p = *self.journal.get(block as usize).ok_or(FsError::Corrupt)? as usize * BS;
        Ok(self.disk.get(p..p + BS).ok_or(FsError::IoError)?.to_vec())
    }
    fn write_journal(&mut self, block: u32, data: &[u8]) -> Result<(), FsError> {
        let p = *self.journal.get(block as usize).ok_or(FsError::Corrupt)? as usize * BS;
        self.disk.get_mut(p..p + BS).ok_or(FsError::IoError)?.copy_from_slice(data); Ok(())
    }
    fn write_home(&mut self, block: u32, data: &[u8]) -> Result<(), FsError> {
        let p = block as usize * BS;
        self.disk.get_mut(p..p + BS).ok_or(FsError::IoError)?.copy_from_slice(data); Ok(())
    }
    fn set_recovery(&mut self, required: bool) -> Result<(), FsError> {
        let at = 1024 + 96;
        let flags = u32::from_le_bytes(self.disk[at..at + 4].try_into().unwrap());
        self.disk[at..at + 4].copy_from_slice(&(if required { flags | 4 } else { flags & !4 }).to_le_bytes());
        Ok(())
    }
    fn flush(&mut self) -> Result<(), FsError> { Ok(()) }
    fn home_is_allowed(&self, block: u32) -> bool { !self.journal.contains(&block) }
}

/// Fail the Nth I/O operation, retaining every earlier write as a crash image.
struct Fault<'a> { inner: &'a mut Inner, step: usize, fail_at: usize }
impl Fault<'_> {
    fn next(&mut self) -> Result<(), FsError> {
        self.step += 1;
        if self.step == self.fail_at { Err(FsError::IoError) } else { Ok(()) }
    }
}
impl JournalIo for Fault<'_> {
    fn read_journal(&mut self, block: u32) -> Result<Vec<u8>, FsError> { self.next()?; self.inner.read_journal(block) }
    fn write_journal(&mut self, block: u32, data: &[u8]) -> Result<(), FsError> { self.next()?; self.inner.write_journal(block, data) }
    fn write_home(&mut self, block: u32, data: &[u8]) -> Result<(), FsError> { self.next()?; self.inner.write_home(block, data) }
    fn set_recovery(&mut self, required: bool) -> Result<(), FsError> { self.next()?; self.inner.set_recovery(required) }
    fn flush(&mut self) -> Result<(), FsError> { self.next()?; self.inner.flush() }
    fn home_is_allowed(&self, block: u32) -> bool { self.inner.home_is_allowed(block) }
}

fn check_crashes(
    fs: &Ext2Filesystem<StagedDevice>, shared: &Arc<Spinlock<Inner>>, image: &std::path::Path,
    paths: &[&str], before: &[Option<&[u8]>], after: &[Option<&[u8]>],
) {
    fs.sync().unwrap();
    let state = shared.lock();
    let original = state.disk.clone();
    let mapping = state.journal.clone();
    let staged = state.staged.clone().into_blocks();
    let blocks_count = (original.len() / BS) as u32;
    let uuid: [u8; 16] = original[1024 + 104..1024 + 120].try_into().unwrap();
    let make_inner = || Inner {
        disk: original.clone(), staged: WriteSet::new(BS, blocks_count, 120).unwrap(), journal: mapping.clone(),
    };
    drop(state);
    let mut baseline = make_inner();
    let mut count = Fault { inner: &mut baseline, step: 0, fail_at: usize::MAX };
    writer::commit(&mut count, BS, mapping.len() as u64, blocks_count, 1, &uuid, &staged).unwrap();
    let steps = count.step;
    for fail_at in 1..=steps {
        let mut crashed = make_inner();
        let mut fault = Fault { inner: &mut crashed, step: 0, fail_at };
        assert_eq!(writer::commit(&mut fault, BS, mapping.len() as u64, blocks_count, 1, &uuid, &staged), Err(FsError::IoError));
        std::fs::write(image, &crashed.disk).unwrap();
        let repair = Command::new("e2fsck").args(["-fy", "-E", "journal_only"]).arg(image).output().unwrap();
        assert!(matches!(repair.status.code(), Some(0 | 1)), "crash {fail_at}: {}", std::string::String::from_utf8_lossy(&repair.stdout));
        let verify = Command::new("e2fsck").args(["-fn"]).arg(image).output().unwrap();
        assert!(verify.status.success(), "crash {fail_at}: {}", std::string::String::from_utf8_lossy(&verify.stdout));
        let actual: Vec<Option<Vec<u8>>> = paths.iter().map(|path| {
            let command = std::format!("cat {path}");
            let read = Command::new("debugfs").args(["-R", &command]).arg(image).output().unwrap();
            if read.stdout.is_empty() { None } else { Some(read.stdout) }
        }).collect();
        let matches = |expected: &[Option<&[u8]>]| actual.iter().zip(expected).all(|(actual, expected)| actual.as_deref() == *expected);
        assert!(matches(before) || matches(after), "crash {fail_at}: {actual:?}");
    }
}

fn commit_staged(fs: &Ext2Filesystem<StagedDevice>, shared: &Arc<Spinlock<Inner>>, image: &std::path::Path) {
    fs.sync().unwrap();
    let mut state = shared.lock();
    let blocks_count = (state.disk.len() / BS) as u32;
    let replacement = WriteSet::new(BS, blocks_count, 120).unwrap();
    let staged = core::mem::replace(&mut state.staged, replacement).into_blocks();
    assert!(!staged.is_empty());
    let uuid: [u8; 16] = state.disk[1024 + 104..1024 + 120].try_into().unwrap();
    let journal_blocks = state.journal.len() as u64;
    writer::commit(&mut *state, BS, journal_blocks, blocks_count, 1, &uuid, &staged).unwrap();
    std::fs::write(image, &state.disk).unwrap();
    drop(state);
    let result = Command::new("e2fsck").args(["-fn"]).arg(image).output().unwrap();
    assert!(result.status.success(), "{}", std::string::String::from_utf8_lossy(&result.stdout));
}

#[test]
fn staged_ext3_mutations_survive_journal_commit_and_fsck() {
    let _serial = crate::tests::pin_test_serial();
    let dir = std::env::temp_dir().join(std::format!("akuma-ext3-write-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let image = dir.join("disk.img");
    std::fs::File::create(&image).unwrap().set_len(32 * 1024 * 1024).unwrap();
    assert!(Command::new("mke2fs").args(["-q", "-F", "-t", "ext3", "-b", "1024"]).arg(&image).status().unwrap().success());
    let disk = std::fs::read(&image).unwrap();
    let count = (disk.len() / BS) as u32;
    let shared = Arc::new(Spinlock::new(Inner { disk, staged: WriteSet::new(BS, count, 120).unwrap(), journal: Vec::new() }));
    let fs = Ext2Filesystem::new_internal(StagedDevice(shared.clone()), || 0, None, MountMode::ReadWrite, false, true).unwrap();
    {
        let state = fs.read_state();
        let inode = fs.read_inode(&state, 8).unwrap();
        let count = inode.file_size() / BS as u64;
        let mut map = Vec::new();
        for logical in 0..count { map.push(fs.get_block_num(&state, &inode, logical as u32).unwrap().unwrap()); }
        shared.lock().journal = map;
    }
    fs.create_dir("/test").unwrap(); commit_staged(&fs, &shared, &image);
    fs.write_file("/test/a", b"journaled file contents\n").unwrap();
    let contents: &[u8] = b"journaled file contents\n";
    check_crashes(&fs, &shared, &image, &["/test/a"], &[None], &[Some(contents)]);
    commit_staged(&fs, &shared, &image);
    fs.rename("/test/a", "/test/b").unwrap();
    check_crashes(&fs, &shared, &image, &["/test/a", "/test/b"], &[Some(contents), None], &[None, Some(contents)]);
    commit_staged(&fs, &shared, &image);
    fs.link("/test/b", "/test/c").unwrap(); commit_staged(&fs, &shared, &image);
    fs.remove_file("/test/b").unwrap();
    check_crashes(&fs, &shared, &image, &["/test/b", "/test/c"], &[Some(contents), Some(contents)], &[None, Some(contents)]);
    commit_staged(&fs, &shared, &image);
    fs.truncate("/test/c", 9).unwrap();
    check_crashes(&fs, &shared, &image, &["/test/c"], &[Some(contents)], &[Some(b"journaled")]);
    commit_staged(&fs, &shared, &image);
    assert_eq!(fs.read_file("/test/c").unwrap(), b"journaled");
    let output = Command::new("debugfs").args(["-R", "cat /test/c"]).arg(&image).output().unwrap();
    assert_eq!(output.stdout, b"journaled");
    std::fs::remove_dir_all(dir).unwrap();
}

/// The production transaction device is exercised here under explicit,
/// test-only operation bracketing. VFS does not yet provide this bracketing.
#[test]
fn transaction_device_commits_filesystem_operations_to_ext3() {
    let _serial = crate::tests::pin_test_serial();
    use super::journal::device::{JournaledDevice, JournalStorage};
    use alloc::collections::BTreeSet;
    #[derive(Clone)]
    struct Disk(Arc<Spinlock<Vec<u8>>>);
    impl BlockDevice for Disk {
        fn read_bytes(&self, offset: u64, out: &mut [u8]) -> Result<(), ()> {
            let bytes = self.0.lock(); let start = usize::try_from(offset).map_err(|_| ())?;
            let end = start.checked_add(out.len()).ok_or(())?;
            out.copy_from_slice(bytes.get(start..end).ok_or(())?); Ok(())
        }
        fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
            let mut bytes = self.0.lock(); let start = usize::try_from(offset).map_err(|_| ())?;
            let end = start.checked_add(data.len()).ok_or(())?;
            bytes.get_mut(start..end).ok_or(())?.copy_from_slice(data); Ok(())
        }
        fn flush(&self) -> Result<(), ()> { Ok(()) }
    }
    impl JournalStorage for Disk { fn atomic_write_bytes(&self) -> usize { 512 } }

    let dir = std::env::temp_dir().join(std::format!("akuma-ext3-device-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let image = dir.join("disk.img");
    std::fs::File::create(&image).unwrap().set_len(32 * 1024 * 1024).unwrap();
    assert!(Command::new("mke2fs").args(["-q", "-F", "-t", "ext3", "-b", "1024"]).arg(&image).status().unwrap().success());
    let disk = Disk(Arc::new(Spinlock::new(std::fs::read(&image).unwrap())));
    let (journal, protected, journal_inode, uuid) = {
        let fs = Ext2Filesystem::new_with_options(disk.clone(), || 0, None, MountMode::ReadOnly).unwrap();
        let state = fs.read_state();
        let inode = fs.read_inode(&state, 8).unwrap();
        let mut map = Vec::new();
        for logical in 0..inode.file_size() / BS as u64 {
            map.push(fs.get_block_num(&state, &inode, logical as u32).unwrap().unwrap());
        }
        fn protect_indirect<B: BlockDevice>(fs: &Ext2Filesystem<B>, state: &Ext2State, block: u32, depth: u8, out: &mut BTreeSet<u32>) {
            if block == 0 { return; }
            assert!(out.insert(block));
            if depth > 1 {
                let data = fs.read_block(state, block).unwrap();
                for entry in data.chunks_exact(4) {
                    let child = u32::from_le_bytes(entry.try_into().unwrap());
                    if child != 0 { protect_indirect(fs, state, child, depth - 1, out); }
                }
            }
        }
        let mut protected = BTreeSet::new();
        protect_indirect(&fs, &state, inode.indirect_block, 1, &mut protected);
        protect_indirect(&fs, &state, inode.double_indirect_block, 2, &mut protected);
        protect_indirect(&fs, &state, inode.triple_indirect_block, 3, &mut protected);
        let index = 7u32;
        let group = index / state.inodes_per_group;
        let in_group = index % state.inodes_per_group;
        let bgd = fs.read_bgd(&state, group).unwrap();
        let start = u64::from(bgd.inode_table) * BS as u64 + u64::from(in_group) * u64::from(state.inode_size);
        (map, protected, start..start + u64::from(state.inode_size), state.superblock.uuid)
    };
    let dev = JournaledDevice::new(disk.clone(), BS, 32768, journal.clone(), protected.clone(), journal_inode.clone(), uuid).unwrap();
    let fs = Ext2Filesystem::new_internal(dev, || 0, None, MountMode::ReadWrite, false, true).unwrap();
    fs.dev.begin().unwrap(); fs.create_dir("/test").unwrap(); fs.sync().unwrap();
    let before = disk.0.lock().clone();
    fs.dev.begin().unwrap(); fs.write_file("/test/a", b"transaction device\n").unwrap();
    assert_eq!(*disk.0.lock(), before, "staging must not write the backing image");
    assert_eq!(fs.read_file("/test/a").unwrap(), b"transaction device\n");
    fs.sync().unwrap();
    fs.dev.begin().unwrap(); fs.rename("/test/a", "/test/b").unwrap(); fs.sync().unwrap();
    assert_eq!(fs.read_file("/test/b").unwrap(), b"transaction device\n");
    std::fs::write(&image, &*disk.0.lock()).unwrap();
    let result = Command::new("e2fsck").args(["-fn"]).arg(&image).output().unwrap();
    assert!(result.status.success(), "{}", std::string::String::from_utf8_lossy(&result.stdout));
    let result = Command::new("debugfs").args(["-R", "cat /test/b"]).arg(&image).output().unwrap();
    assert_eq!(result.stdout, b"transaction device\n");
    drop(fs);
    let fs = super::journal::filesystem::JournaledFilesystem::mount(disk.clone(), || 0, None).unwrap();
    fs.create_dir("/automatic").unwrap();
    fs.write_file("/automatic/a", b"automatic transaction\n").unwrap();
    fs.rename("/automatic/a", "/automatic/b").unwrap();
    assert_eq!(fs.read_file("/automatic/b").unwrap(), b"automatic transaction\n");
    std::thread::scope(|scope| {
        for index in 0..4 {
            let fs = &fs;
            scope.spawn(move || {
                let path = std::format!("/automatic/thread{index}");
                fs.write_file(&path, b"parallel\n").unwrap();
                assert_eq!(fs.read_file(&path).unwrap(), b"parallel\n");
            });
        }
    });
    std::fs::write(&image, &*disk.0.lock()).unwrap();
    assert!(Command::new("e2fsck").args(["-fn"]).arg(&image).output().unwrap().status.success());
    let xattr = Command::new("debugfs").args(["-w", "-R", "ea_set /automatic/b user.example value"]).arg(&image).output().unwrap();
    assert!(xattr.status.success());
    let xattr_disk = Disk(Arc::new(Spinlock::new(std::fs::read(&image).unwrap())));
    let xattr_fs = super::journal::filesystem::JournaledFilesystem::mount(xattr_disk, || 0, None).unwrap();
    assert_eq!(xattr_fs.read_file("/automatic/b"), Err(FsError::NotSupported));
    let mut dirty = disk.0.lock().clone();
    dirty[1024 + 96] |= 4;
    let dirty_disk = Disk(Arc::new(Spinlock::new(dirty.clone())));
    assert!(matches!(super::journal::filesystem::JournaledFilesystem::mount(dirty_disk.clone(), || 0, None), Err(FsError::NotSupported)));
    assert_eq!(*dirty_disk.0.lock(), dirty, "dirty writable mounts must not modify the disk");
    let pinned_fs = super::journal::filesystem::JournaledFilesystem::mount(disk.clone(), || 0, None).unwrap();
    let inode = pinned_fs.resolve_inode("/automatic/b").unwrap();
    let pin = akuma_primitives::InodePin::new(inode);
    let before_pin = disk.0.lock().clone();
    assert_eq!(pinned_fs.remove_file("/automatic/b"), Err(FsError::NotSupported));
    assert_eq!(*disk.0.lock(), before_pin, "pinned unlink must not reach the disk without orphan tracking");
    drop(pin);
    let second = super::journal::filesystem::JournaledFilesystem::mount(disk.clone(), || 0, None).unwrap();
    second.simulate_dead_gate_holder(7);
    assert!(second.abandon_tid(7));
    assert_eq!(second.read_file("/automatic/b"), Err(FsError::IoError));
    let before = disk.0.lock().clone();
    assert_eq!(fs.remove_file("/missing"), Err(FsError::NotFound));
    assert_eq!(*disk.0.lock(), before, "failed mutation must not reach home blocks");
    assert_eq!(fs.read_file("/automatic/b").unwrap(), b"automatic transaction\n", "a failure before the write lock must not poison the filesystem");
    assert_eq!(fs.remove_file("/automatic"), Err(FsError::NotAFile));
    assert_eq!(fs.read_file("/automatic/b"), Err(FsError::IoError), "cached reads must not survive an uncertain abort");
    assert!(!fs.exists("/automatic/b"));
    assert_eq!(fs.sync(), Err(FsError::IoError));
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(feature = "ext4")]
#[test]
fn constrained_ext4_journals_existing_extent_overwrites_and_new_legacy_files() {
    use super::journal::{device::JournalStorage, filesystem::JournaledFilesystem};
    let _serial = crate::tests::pin_test_serial();
    #[derive(Clone)]
    struct Disk(Arc<Spinlock<Vec<u8>>>);
    impl BlockDevice for Disk {
        fn read_bytes(&self, offset: u64, out: &mut [u8]) -> Result<(), ()> {
            let data = self.0.lock(); let start = usize::try_from(offset).map_err(|_| ())?;
            let end = start.checked_add(out.len()).ok_or(())?;
            out.copy_from_slice(data.get(start..end).ok_or(())?); Ok(())
        }
        fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
            let mut bytes = self.0.lock(); let start = usize::try_from(offset).map_err(|_| ())?;
            let end = start.checked_add(data.len()).ok_or(())?;
            bytes.get_mut(start..end).ok_or(())?.copy_from_slice(data); Ok(())
        }
        fn flush(&self) -> Result<(), ()> { Ok(()) }
    }
    impl JournalStorage for Disk { fn atomic_write_bytes(&self) -> usize { 512 } }
    let dir = std::env::temp_dir().join(std::format!("akuma-ext4-write-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let image = dir.join("disk.img"); let input = dir.join("input");
    std::fs::File::create(&image).unwrap().set_len(32 * 1024 * 1024).unwrap();
    std::fs::write(&input, b"original contents\n").unwrap();
    assert!(Command::new("mke2fs").args(["-q", "-F", "-t", "ext4", "-b", "1024", "-O", "^64bit,^metadata_csum,^flex_bg,^huge_file,^dir_nlink,^extra_isize,^uninit_bg"]).arg(&image).status().unwrap().success());
    let command = std::format!("write {} /existing", input.display());
    assert!(Command::new("debugfs").args(["-w", "-R", &command]).arg(&image).output().unwrap().status.success());
    let disk = Disk(Arc::new(Spinlock::new(std::fs::read(&image).unwrap())));
    let fs = JournaledFilesystem::mount(disk.clone(), || 0, None).unwrap();
    assert_eq!(fs.name(), "ext4");
    assert_eq!(fs.write_at("/existing", 0, b"new"), Ok(3));
    fs.create_dir("/dir").unwrap();
    fs.write_file("/dir/a", b"new legacy file\n").unwrap();
    fs.rename("/dir/a", "/dir/b").unwrap();
    assert_eq!(fs.read_file("/dir/b").unwrap(), b"new legacy file\n");
    assert_eq!(fs.read_file("/existing").unwrap(), b"newginal contents\n");
    std::fs::write(&image, &*disk.0.lock()).unwrap();
    let check = Command::new("e2fsck").arg("-fn").arg(&image).output().unwrap();
    assert!(check.status.success(), "{}", std::string::String::from_utf8_lossy(&check.stdout));
    let read = Command::new("debugfs").args(["-R", "cat /dir/b"]).arg(&image).output().unwrap();
    assert_eq!(read.stdout, b"new legacy file\n");
    let reject_fs = JournaledFilesystem::mount(disk.clone(), || 0, None).unwrap();
    let before_hole = disk.0.lock().clone();
    assert_eq!(reject_fs.write_at("/existing", 1024, b"x"), Err(FsError::NotSupported));
    assert_eq!(*disk.0.lock(), before_hole, "unsupported extent growth must not modify the image");
    // Truncating an extent-mapped file is rejected before any disk change.
    let before = disk.0.lock().clone();
    assert_eq!(fs.truncate("/existing", 0), Err(FsError::NotSupported));
    assert_eq!(*disk.0.lock(), before);
    assert_eq!(fs.read_file("/dir/b"), Err(FsError::IoError));
    std::fs::remove_dir_all(dir).unwrap();
}
