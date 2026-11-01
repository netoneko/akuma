# `.clone()` audit: amd64 + Taoxichuan kernels and their workspace crates

Tree: branch `rust-org-il-experiment` @ `e2d3bdc1` plus the uncommitted working-tree changes, 2026-11-01. Read-only audit; nothing in the repo was edited.

## 1. Method

### 1.1 Mechanical pass (clippy)

```
cargo clippy -p taoxichuan -p akuma-amd64 --release --target x86_64-unknown-none --message-format=short -- \
  -W clippy::redundant_clone -W clippy::clone_on_copy -W clippy::clone_on_ref_ptr -W clippy::implicit_clone \
  -W clippy::unnecessary_to_owned -W clippy::assigning_clones -W clippy::cloned_instead_of_copied
```

102 workspace crates were checked; every one emitted the unknown-lint probe warning, which proves the run applied these arguments and did not replay a cache.

| lint | hits |
|---|---:|
| `redundant_clone`, `clone_on_copy`, `implicit_clone`, `unnecessary_to_owned`, `assigning_clones`, `cloned_instead_of_copied` | **0** |
| `clone_on_ref_ptr` (`x.clone()` on an `Arc` → spell it `Arc::clone(&x)`) | 30 |

The 30 `clone_on_ref_ptr` hits only ask for a different spelling of a refcount bump; none of them allocates. They are: akuma-exec 19, akuma-vfs 4, akuma-isolation 2, akuma-syscalls-glue 2, akuma-vfs-glue 1, amd64 2, taoxichuan 1. The point that matters is that clippy found **no** defect of the kind this audit is looking for. Every finding below comes from the manual pass. The defects are semantic: an owned deep copy made to test a variant or read one field, a copy whose original is dropped on the next line, an allocation made inside an IRQ-masked lock hold. The `redundant_clone` MIR analysis does not model any of these.

### 1.2 Counting (reproducible)

Script: `a throwaway `count_clones.py` (not kept; `scripts/cloc_akuma.py`'s `copy_sites` replaced it)`. The component list it reads is `components.txt`. A variant that takes the regex from the environment variable `CLONE_PAT` is `count_pat.py`.

What the counter does:

- **Files.** It reads every `*.rs` under `<component>/src/`. It skips `tests/`, `benches/`, `examples/` and `build.rs`, and it skips a library crate's `src/main.rs` and `src/bin/`. The only file in scope that this rule drops is `crates/akuma-locks-rw-cell/src/main.rs`. `amd64/src/main.rs` and `taoxichuan/src/main.rs` are the kernels and are counted.
- **Lexing.** Before matching, it blanks `//` and `/* */` comments (nested), string literals (`"…"`, `b"…"`, `r#"…"#`) and char literals, so a lifetime such as `'a` is not mistaken for a char literal.
- **Test code.** It drops the item that follows `#[cfg(test)]` or `#[cfg(all(test, …))]`, whether that item is a braced body or ends in `;`. When the item is `mod x;`, it also drops `x.rs` and `x/`.
  - `#[cfg(any(test, …))]` is **kept**, because that code is production under the other predicate. Two such sites exist (`akuma-net-rtl8169` and `akuma-trap-amd64`); neither contains a clone.
  - `#[cfg(kernel_tests)]` and boot-suite helpers are **kept**, because they compile into production images.
- **Pattern.** A site matches `\.\s*clone\s*\(\s*\)`. `.cloned()`, `.to_vec()`, `.to_string()`, `X::clone(` and `.get_fd(` are counted as separate columns.
- **Arch-gated code is counted.** Code under `cfg(target_arch = "aarch64")` and code reachable only from the AArch64 kernel is included. That suits a budget, which counts source, not reachability. The findings below say per site whether the amd64 kernels actually run it.

A naive `grep -c '\.clone()'` gives **235**. The production-only count is **125**. Of the difference, 106 sites are in `cfg(test)` code and 4 are in comments or strings (3 in akuma-exec, 1 in akuma-syscalls-glue).

### 1.3 Per-component production counts (sorted by `.clone()`)

| component | `.clone()` prod | naive grep | in cfg(test) | `.cloned()` | `.to_vec()` | `.to_string()` | `.get_fd(` † |
|---|---:|---:|---:|---:|---:|---:|---:|
| crates/akuma-exec | 47 | 54 | 4 | 2 | 4 | 6 ‡ | 1 |
| crates/akuma-syscalls-glue | 23 | 24 | 0 | 0 | 0 | 0 | 60 |
| amd64 | 17 | 17 | 0 | 1 | 0 | 0 | 0 (`table_get(` ×6 callers) |
| crates/akuma-fdtable | 8 | 8 | 0 | 1 | 0 | 0 | 0 |
| crates/akuma-vfs | 7 | 7 | 0 | 0 | 1 | 0 | 0 |
| crates/akuma-isolation | 6 | 32 | 26 | 2 | 0 | 0 | 0 |
| crates/akuma-net-unix | 4 | 9 | 5 | 0 | 2 | 0 | 0 |
| crates/akuma-mmap | 3 | 4 | 1 | 0 | 0 | 0 | 0 |
| crates/akuma-vfs-glue | 3 | 3 | 0 | 3 | 7 | 0 | 3 |
| taoxichuan | 3 | 3 | 0 | 0 | 0 | 0 | 0 |
| crates/akuma-exec-core | 1 | 2 | 1 | 0 | 0 | 0 | 0 |
| crates/akuma-syscalls | 1 | 1 | 0 | 0 | 0 | 0 | 0 |
| crates/akuma-syscalls-glue-amd64 | 1 | 1 | 0 | 0 | 0 | 0 | 0 |
| crates/akuma-syscalls-ipc | 1 | 1 | 0 | 0 | 1 | 0 | 0 |
| crates/akuma-ext2 | 0 | 22 | 22 | 0 | 2 | 2 | 0 |
| crates/akuma-locks-rw | 0 | 14 | 14 | 0 | 0 | 0 | 0 |
| crates/akuma-bkl | 0 | 7 | 7 | 0 | 0 | 0 | 0 |
| crates/akuma-procfs | 0 | 6 | 6 | 0 | 0 | 0 | 0 |
| crates/akuma-slot-table | 0 | 4 | 4 | 0 | 0 | 0 | 0 |
| crates/akuma-net-nic, -nvme, -primitives | 0 | 3 each | 3 each | 0 | 0 | 0 | 0 |
| crates/akuma-ryzen-amd64, -svm-amd64 | 0 | 2 each | 2 each | 0 | 0 | 0 | 0 |
| crates/akuma-pmm, -selftest | 0 | 1 each | 1 each | 0 | 0 | 0 | 0 |
| crates/akuma-tty | 0 | 1 | 1 | 0 | 2 | 0 | 0 |
| **total** | **125** | **235** | **106** | **9** | **19** | **8** | **64** |

The other **75 components in scope have zero** production sites in every column.

† `Process::get_fd` (`crates/akuma-exec/src/process/fd.rs:318`) and amd64's `table_get` (`amd64/src/fd.rs:324`) are **deep clones by another name**: `.get(&fd).cloned()` of a `FileDescriptor`. A `.clone()`-only gate cannot see the most expensive clones in the tree, because they sit behind these two wrappers (see F1 and F2). The gate should count `.cloned()` as well, and should either budget calls to these two wrappers or make them return a borrow.

‡ 4 of the 6 are `"test".to_string()` in `make_test_process` (`process/mod.rs:4094`), a boot-test helper compiled into production.

**Copy-type noise:** `clone_on_copy` reports 0 for the whole graph, so there is nothing to summarise per crate.

---

## 2. Background the findings depend on

- `FileDescriptor` (`crates/akuma-exec-core/src/process.rs:179`) has exactly one variant that owns heap data: `File(KernelFile)`.
- `KernelFile` (`:322`) holds:
  - `path: String`;
  - `dir_cache: Option<Vec<DirCacheEntry{name: String}>>`;
  - `pin: InodePin`.
- `InodePin::clone` (`crates/akuma-primitives/src/inode_pin.rs:175`) calls `acquire(inode)`, a CAS insert into a **global** open-addressed table. `Drop` calls `release`.

So cloning a `File` descriptor costs:

- one `String` allocation;
- one global pin-table acquire and one release;
- for a directory fd that has been through `getdents64`, a deep copy of every directory entry. The code itself records ~100k `String`s and "~60 ms of pure allocation per syscall" (`fd.rs:302-309`, the doc on `with_fd`, which says read-only callers "must go through here").

Cloning any other variant is a plain memcpy.

`LazySource::File` (`crates/akuma-exec-core/src/process.rs:410`) has the same shape (`path: String` plus `pin`). So does `SharedWriteBack` (`crates/akuma-mmap/src/region.rs:153`, which has `path: String`).

---

## 3. Findings: clearly unneeded (ranked by severity)

### F1 [HIGH, hot: amd64 syscall dispatch]: `table_get` deep-clones a descriptor to test its variant

`amd64/src/fd.rs:332` is `t.table.lock().get(&(fd as u32)).cloned()`, and it is reached through `table_get`. Every production caller only matches a variant or tests for presence:

- `console_end` (`fd.rs:401`). This is called by `usermode::sys_write` on **every `write(2)`** (`usermode.rs:2969`), by `sys_pread64` (`fd.rs:1316`), by `sys_fstat` (`fd.rs:1868`), by `sys_ioctl` (`fd.rs:2255`), by the poll console hook (`fd.rs:2128`) and by `sendfile` (`fd.rs:2883/2885`).
- `socket_index` (`fd.rs:462`), which serves 10 socket syscalls in `sock.rs`.
- `is_unix_socket` (`fd.rs:483`), called by 11 dispatch arms in `usermode.rs:1817-2433`.
- `pipe_read_id` (`fd.rs:505`) and `pipe_write_id` (`fd.rs:682`).
- the presence check in `sys_dup2` (`fd.rs:1057`).

`sys_pread64` on a file therefore makes **four** full `KernelFile` clones (`fd.rs:1313-1316`) before glue makes a fifth (F2).

**Fix.** Use the borrowing `table_with` that already sits next to it (`fd.rs:342`), e.g. `table_with(fd, |d| matches!(d, FileDescriptor::UnixSocket{..})).unwrap_or(false)`. Keep `table_get` only where an owned copy is the point, which is no current caller.

### F2 [HIGH, hot: every fd syscall amd64 forwards to glue]: `Process::get_fd` deep clone where a borrow or a few `Copy` fields suffice

`get_fd` clones **inside `with_irqs_disabled` and the fd-table lock**, so a `File` clone is a heap allocation with IRQs masked. That is the hazard `CLAUDE.md` § Kernel conventions and the comment at glue `mem.rs:1129` both warn about.

There are 60 call sites in `crates/akuma-syscalls-glue`. amd64 forwards `read`, `write`, `pread64`, `lseek`, `fcntl`, `getdents64`, `fchdir`, `fstatfs` and the `*at` calls to glue. The sites below only test a variant or presence, or read `Copy` fields:

| site | what the result is used for | note |
|---|---|---|
| `fs.rs:2375` **and** `fs.rs:2383` (`sys_lseek`) | `matches!(DevNull|DevZero)`, then `matches!(File|BlockDev)` | **two** deep clones per `lseek`. `seekdir`/`rewinddir` on a directory fd copy its `dir_cache` twice |
| `fs.rs:3234` (`sys_fcntl`) | `.is_none()` | every `fcntl` (`F_GETFD`/`F_SETFD` are very common) |
| `fs.rs:3270` (`F_GETFL`) | `f.flags` (a `u32`) | a second clone on the same call |
| `fs.rs:3131` (`utimensat`) | `.is_some()` | |
| `fs.rs:219` (`dirfd_base`, i.e. every `*at(dirfd, rel)`) | `f.path` | deep-copies the dirfd's `dir_cache`. `openat(dirfd, name)` after `getdents` is exactly how fts, `find`, `rm -r` and `git` walk trees |
| `fs.rs:2950` (statx dirfd), `fs.rs:3515` (linkat), `fs.rs:1652` (fstatfs) | path + inode + mount | |
| `fs.rs:2453` (fstat), `fs.rs:2687` (fchmod), `fs.rs:2763` (fchown), `fs.rs:2812` (fallocate), `fs.rs:2827` (ftruncate), `fs.rs:2896` (fsync) | path + inode + mount | |
| `fs.rs:3743` (fchdir) | path | the target is always a directory, so `dir_cache` is copied too. See also F15 |
| `poll.rs:501`, `poll.rs:1038` | `.is_none()` | per registered fd **per `epoll_pwait` iteration** |
| `poll.rs:538` | variant + `Copy` payload | per fd per poll |
| `poll.rs:94`, `poll.rs:823`, `poll.rs:859` | variant | |
| `term.rs:55`, `term.rs:147`, `term.rs:175`, `term.rs:199`, `term.rs:206`, `term.rs:217`, `term.rs:246`, `term.rs:394` | variant / `Copy` payload | `ioctl` paths |
| `net.rs:1423`, `net.rs:1561`, `net.rs:1577` | variant / `Copy` payload | |

**Fix.** Use `proc.with_fd(fd, |d| …)`, which exists and is documented as the required form for readers, and return only the `Copy` fields plus `f.path.clone()` where a path is really used.

`sys_read` (`fs.rs:312`), `sys_write` (`fs.rs:1023`) and `pread64`/`pwrite64` (`fs.rs:935/987`) are in §4 (J1). They need a small refactor rather than a one-line change.

### F3 [HIGH, every execve]: argv copied 4 times on amd64 and 3 times on Taoxichuan

**amd64:**

1. `usermode.rs:5307` builds `new_args: Vec<String>`. This copy is needed.
2. `install_image` sets `img.args = inst.args.to_vec()` (`crates/akuma-exec/src/process/image.rs:324`), under the image lock.
3. `usermode.rs:5420` makes `new_args.iter().map(|a| a.clone().into_bytes())`. This clone exists only to feed glue.
4. `execve::commit` (`crates/akuma-syscalls-glue/src/execve.rs:127`) rebuilds `img.args` from those bytes. That **overwrites copy 2**, so copy 2 was wasted as well.

**Taoxichuan** (`taoxichuan/src/child.rs:549`): `args.argv.iter().map(|a| a.clone().into_bytes())`, followed by copy 4 again.

**Fix.**

- Make `commit` take `&[String]` (or `impl IntoIterator<Item = &[u8]>`), which removes the intermediate `Vec<Vec<u8>>`.
- Let exactly one of `install_image`/`commit` set `img.args`.
- Make `ImageInstall::args` an owned `Vec<String>` that is moved in. Its only `&[String]`-needing caller, `replace_image`, already owns its argv.

The result is one copy per exec instead of four.

### F4 [HIGH, hot: every terminal write]: `TerminalState::translate_output` copies even when it translates nothing

`crates/akuma-tty/src/session.rs:288` returns `data.to_vec()` when `ONLCR` is off. Its only production caller is glue `sys_write` (`fs.rs:1120`), on every write to a terminal channel. The copy is made while the `TerminalState` spinlock is held. Raw-mode programs (editors, tmux) pay a full copy of every write for nothing.

**Fix.** Return `Cow<'_, [u8]>`, or have the caller test `needs_onlcr()` first. `echo_noncanon` (`session.rs:278`) has the same shape on the per-keystroke echo path.

### F5 [MEDIUM-HIGH, getdents cache fill]: `e.name.clone()` out of a `Vec` that is about to be dropped

`crates/akuma-syscalls-glue/src/fs.rs:3676`: `dir_entries` is owned and never used after the `.chain(dir_entries.iter()…)` that builds the cache. Each `DirCacheEntry` therefore costs a second `String` per directory entry, on every first `getdents64` of every directory.

**Fix.** Use `into_iter()` and move `e.name`. Compute `d_type` (which borrows `&e.name` through `dev_node_named`) before the move. The same code documents 100k-entry directories.

### F6 [MEDIUM-HIGH]: amd64 `madvise(DONTNEED)` clones the path once per present page, with IRQs masked

`amd64/src/mm.rs:2863` and `amd64/src/mm.rs:2868` push `DontneedFlush { path: sw.path.clone(), … }` once per present page of a shared-writable file region. This runs inside `with_current_regions` (`IrqGuard` plus the region lock, `usermode.rs:3691`) and `with_current_address_space` → `rewrite_leaves_in_range`. An N-page `DONTNEED` makes N identical `String` allocations under two spinlocks with IRQs off.

**Fix.** Record the path once per region (for example `Vec<(String, mount, inode)>` plus a per-page index), or make `SharedWriteBack::path` an `Arc<str>` (see S1).

### F7 [MEDIUM, lazy-region lookups]: `LazyRegionMap::lookup` clones `LazySource` (path `String` + pin acquire/release) only to test variant or presence

`lookup` is `crates/akuma-exec/src/process/children.rs:1048`. The callers that discard everything but the variant:

- glue `mem.rs:1139` (`MADV_DONTNEED` pass 0): **once per page**, inside `with_irqs_disabled` plus `lazy_regions.lock()`. The comment ten lines above it bans exactly this allocation-under-lock.
- `mem.rs:1264` (`MADV_WILLNEED`), once per unmapped page.
- `mem.rs:820` (`mremap` `.is_some()`).
- `crates/akuma-exec/src/process/lazy_prefault.rs:34`, once per page; it uses only the variant.
- `children.rs:1694` (`is_in_lazy_region` → `.is_some()`). This function has **no callers** in the tree.

**Fix.** Add a borrowing `LazyRegionMap::get(va) -> Option<&LazyRegion>` (or `is_file_at(va)`) and keep the cloning `lookup` for the fault path, which drops the lock before I/O.

**Reach.** amd64 has its own `mm::sys_madvise` and registers its own prefault hook (`amd64/src/boot.rs:242`). These paths are therefore AArch64-hot, but they compile into both kernels and count against the budget.

### F8 [MEDIUM, munmap/mprotect on lazy regions]: `LazySource` cloned before the branch that decides whether it is needed

**`update_flags`** (`children.rs:1127`). `r_source` is cloned up front:

- In the fully-contained arm it is dropped unused.
- In the partial arm, `self.regions.remove(&key)` returns the owned region and throws it away, then the pieces clone `r_source` twice more (`children.rs:1143`, `children.rs:1150`).

**`munmap_one_overlap`** (`children.rs:1186`). `reg_source` is cloned up front:

- It is wasted in the `'F'` (full) and `'S'` (suffix) arms.
- In the `'P'` arm the removed region's source could be moved instead.
- Only the `'M'` (middle split) arm needs a real copy.

**Fix.** Take the source from `remove()`, and clone only for the extra piece.

### F9 [MEDIUM, per file mmap]: double path copies in glue's `mmap`

All of these clone the whole `KernelFile` through `get_fd` first.

- `crates/akuma-syscalls-glue/src/mem.rs:443` and `mem.rs:590`: `if let Some(File(ref f)) = proc.get_fd(..) { let path = f.path.clone(); … }`. The descriptor is already an owned clone; bind it by value and move `f.path`.
- `mem.rs:600`: `LazySource::file(path.clone(), …)`. After that, `path` is used only for the shared-writable record (`mem.rs:616`, another `path.clone()`) and a const-gated trace.

On the lazy-file path this is up to four `String` copies where one (two when shared-writable) suffice. `mem.rs:757` follows the same pattern.

Better still, use `with_fd` to extract `(path, mount_id, inode)` (F2). This is the AArch64 `mmap` path; amd64 has `mm::sys_mmap`.

### F10 [MEDIUM, per successful openat on both glue users]: `KernelFile::new(path.clone(), flags)`

`crates/akuma-syscalls-glue/src/fs.rs:2237` is in `openat_path`, which amd64 reaches through `akuma-syscalls-glue-amd64`'s `openat`. After this line `path` is used only for `open_file_ids(&path)` (`fs.rs:2238`) and a trace gated on `SYSCALL_DEBUG_IO_ENABLED`.

**Fix.** Call `open_file_ids` first and move `path`. Because the allocation is a side effect, the compiler cannot elide the clone even when the trace const is false.

### F11 [MEDIUM, every `*at()` with a relative path]: cwd cloned in order to be joined

`fs.rs:210` (`dirfd_base` with `AT_FDCWD`) returns `proc.cwd.clone()`. `resolve_path_at` (`fs.rs:238`) then allocates the joined path, so that is two allocations where one suffices.

**Fix.** Join directly from `&proc.cwd`. Today's clone reads the same field just as unsynchronised, so a borrow is no weaker.

`crates/akuma-vfs-glue/src/lib.rs:394` has the same shape in `resolve_mount`'s spawn-namespace override (cold, but it allocates while holding `SPAWN_NS_OVERRIDE`).

### F12 [MEDIUM, spawn into a box / container syscalls]: `registry_snapshot` deep-clones the box registry for a pure decision

`crates/akuma-isolation/src/box_registry/mod.rs:133` (`BOX_REGISTRY.lock().clone()`, under IRQ mask) copies every `BoxInfo` (`name` and `root_dir` `String`s). Its callers:

- glue `proc.rs:1811` (spawn with `box_id`)
- `proc.rs:1891`
- `container.rs:29` and `container.rs:59`
- `crates/akuma-exec/src/process/mod.rs:954`

All five feed `can_access_box` or `cascade_kill_order`, which are pure (`box_registry/access.rs:19-41`, `access.rs:129`). `can_access_box` returns `true` immediately for `source_box_id == 0`, so box-0 callers, the common case, pay for the copy and never look at it.

**Fix.** A `with_registry(|r| …)` borrow. `list_boxes` (`mod.rs:68`) is a legitimate snapshot (`/proc` render).

### F13 [MEDIUM, SysV msgsnd]: the payload is copied on the success path while the original is dropped

`crates/akuma-syscalls-ipc/src/lib.rs:249` does `KernelMsg { mtype, data: data.clone() }`, where `data` is the `Vec` copied from user space. The copy happens inside `with_irqs_disabled` plus `MSGQUEUE_TABLE.lock()`. That branch returns, and `data` is dropped right after.

**Fix.** `core::mem::take(&mut data)` on the success branch. The retry branch does not consume it.

### F14 [LOW-MEDIUM, every overlay path component in containers]: `let parent = prefix.clone(); prefix = join(&parent, comp);`

`crates/akuma-isolation/src/overlay_fs.rs:208`. **Fix:** `let parent = core::mem::take(&mut prefix);`, which has identical semantics and makes one allocation fewer per path component.

### F15 [LOW, per call; cold]: smaller clear cases

- **Taoxichuan `fd_inode`** (`taoxichuan/src/main.rs:1073`) clones the whole `KernelFile` to read `inode()`; the path is used only when `inode == 0`. This is the same shape amd64 already fixed in `file_bytes_at` (`amd64/src/fd.rs:1384-1391`, after a documented 76,800-allocation incident). It runs on every file `mmap` (every `PT_LOAD` musl ldso maps).
  - **Fix:** `Some((f.inode(), (f.inode() == 0).then(|| f.path.clone())))` inside the closure.
- **glue `sys_fchdir`** (`fs.rs:3755`): `let new_cwd = path.clone();` exists only so the const-gated debug print can show `path` after the move. Print first, or print from `p.cwd`.
- **akuma-mmap `munmap` clip** (`crates/akuma-mmap/src/region.rs:720`): `regions[i].shared_write.clone()` is used only as `.as_ref().map(|sw| sw.advance(..))` (`region.rs:762`). Call `advance` on `regions[i].shared_write.as_ref()` directly; `advance` already makes its own copy.
- **akuma-mmap `mprotect` split** (`region.rs:599`): the head piece clones `shared_write`, which it owns. Compute the mid/tail `advance`s first, then move the original into the head.
- **glue `flush_and_clear_shared_file_mappings`** (`mem.rs:307`): it snapshots with `path: m.path.clone()`, flushes, then `retain()`s the same entries out. Drain them by value instead (use `retain` with `core::mem::take(&mut v.path)` into the output `Vec`). This is AArch64 exit; `mem.rs:335`/`mem.rs:1532` are different and are listed in §5.
- **akuma-exec `dump_running_process_stats`** (`stats.rs:152`) clones every process name and then uses only those that have run more than 10 s; it already re-looks-up each pid. This is debug-gated. `kill_children_whose_parent_in` (`mod.rs:1825`) carries a `String` per child for a trace capped at 512 prints.
- **amd64 `sys_spawn`** (`usermode.rs:5739`): `p.channel.clone()` exists only for a pointer comparison. Use `.as_ref().is_some_and(|ch| is_console_channel(ch))`. This is a refcount bump, not an allocation.
- **amd64 `run_init`** (`usermode.rs:8863`): `&INIT_CMDLINE.lock().clone()`. `INIT_CMDLINE` is read nowhere else (its three references are at `usermode.rs:4340`, `:4364` and `:8863`), so `core::mem::take` works. It runs once at boot.
- **akuma-fdtable `dup`/`dup_min`/`dup_to`** (`crates/akuma-fdtable/src/lib.rs:258-259`, `284-287`, `309-310`) store one clone and **return a second** as an "effect token". No production code calls these three (glue `sys_dup`/`sys_dup3` use `get_fd` + `alloc_fd`, `fs.rs:1722/1743`), so they cost 6 budget sites for nothing. Return the fd and let the caller borrow the entry.

---

## 4. Plausible: needs a judgment call

### J1. Glue `sys_read`/`sys_write`/`pread64`/`pwrite64` snapshot the whole descriptor

Sites: `fs.rs:312`, `fs.rs:1023`, `fs.rs:935`, `fs.rs:987`. The snapshot exists so that no lock and no `&'static Process` is held across a park (the comment at `fs.rs:1018`). But the `File` arms use only:

- `inode`, `mount_id`, `position` and `flags`, all `Copy`;
- `path`, which `read_at_open_file` reads only when `inode == 0` (`akuma-vfs-glue/src/lib.rs:674`).

A small `Copy` "view" enum extracted with `with_fd`, carrying a path only when `inode == 0`, would make `read(2)`/`write(2)` on a regular file allocation- and pin-free. This is the hottest path in this audit. It is a refactor of two long functions, not a one-liner.

### J2. `LazyRegionMap::lookup` on the AArch64 page-fault path

`children.rs:1048`, reached through `lazy_region_lookup_for_page_fault`. The cloned pin is defensible: it keeps the inode alive after the lock drops. The path `String` is probably unneeded, because the fill reads by `(mount_id, inode)`.

Option: `Arc<str>` for `LazySource::File::path` (S1), or drop the path from what the fault path copies. The fault handler itself is in `akuma-exceptions`, which is outside this scope.

### J3. `Process::inherit_from` for `clone_thread` (and `vfork`)

Site: `crates/akuma-exec/src/process/mod.rs:1005-1034`.

- **`memory: parent.memory.clone()`** copies the parent's `free_regions` `Vec` under its lock. A `CLONE_VM` thread's free list is never consulted: `alloc_mmap`/`remove_mmap_region` resolve the owner through `address_space_owner_pid_for_fault` (`children.rs:969-1000`, `1697-1710`), and amd64 resolves through `current_mm_process`. A vfork child's list is reset at exec.
- **`name`/`exe`/`args` and `cwd`/`root`** are deep-copied per thread. `args` is a `Vec<String>`, so every `pthread_create` in Go, tokio or rustc copies argv.

Options: an `InheritOverrides` field choosing a fresh `ProcessMemory` for threads, and `Arc<[String]>` for args.

This is a judgment call because each thread is a full `Process` whose `/proc/<tid>/cmdline` and cwd are read from its own fields today. Sharing changes who answers those reads.

### J4. Path clones whose only purpose is a VFS call after the lock drops

- amd64 `file_identity` (`fd.rs:1423`): the path is used only in `metadata_open_file`'s error fallback, which the code says "is not a case that exists today".
- amd64 `is_regular_file` (`fd.rs:1491`) and its copy in `crates/akuma-syscalls-glue-amd64/src/lib.rs:202`: a path clone for `path_is_dir`. A by-inode `metadata_by_inode(..).is_dir` would avoid it. Note also that these two functions are near-duplicates.

### J5. fork's `mmap_snapshot`

`crates/akuma-exec/src/process/mod.rs:3046` makes `parent.mmap_regions.lock().clone()`, deep-copying every region's `frames` `Vec` so that the page copying runs without the lock. The copy is lock scoping, but only `(start_va, frame addrs, prot, prot_recorded)` is read. It is AArch64 eager fork only (`fork_share_parent_memory`); amd64 has its own fork. Keep it, or narrow it to a lighter snapshot.

### J6. `flock` key clone with IRQs masked

`crates/akuma-syscalls-glue/src/flock.rs:104` inserts `path.clone()` inside `with_irqs_disabled` plus `FLOCK_TABLE.lock()`, in a retry loop. The key has to be owned, but the allocation could move out of the masked hold, for example with an `Option<String>` that is taken on the first insert. The `BTreeMap` node allocation would still happen under the lock.

### J7. Overlay `read_dir` `seen.push(e.name.clone())`

`crates/akuma-isolation/src/overlay_fs.rs:425` keeps a copy of every name next to the moved `DirEntry`. It could check `out` plus a whiteout-only set. `contains()` is O(n²) regardless.

### J8. By-value fd model on fork and dup

`fork_copy` (`akuma-fdtable/src/lib.rs:394`) and `dup` through `get_fd` clone each `File` descriptor, including its `dir_cache`. This is required by the current model, where a dup's cursor is a pinned divergence and is copied. Making the payload `Arc`-shared would remove the copies and the divergence together. That is a design change, not a clone fix.

---

## 5. Clones that look unneeded but are required (do not "fix")

- **An `Arc` returned out of a lock-protected static or table.** The lock cannot outlive the call.
  - amd64 `console.rs:192/198`
  - `akuma-exec`: `channel.rs:332`, `channel.rs:581`, `children.rs:29`, `children.rs:236`, `children.rs:340`, `children.rs:964`, `exec.rs:248`
  - `akuma-vfs-glue`: `lib.rs:85/106`
  - `akuma-isolation`: `lib.rs:46/49`
  - glue `term.rs:152`
- **`akuma-vfs` `mount.rs:294/316/403/424`.** The resolver hands out an owned `Arc<dyn Filesystem>` *so that* no I/O happens under the mount-table lock. That is the documented discipline (`akuma-vfs-glue/src/lib.rs:385`). Turning it into a borrow reintroduces the `MOUNT_TABLE` lock inversion that the CLAUDE.md `akuma-locks-rw` note describes.
- **`Arc` handed to more than one owner.**
  - `spawn.rs:435/567/584`: the channel goes into the process, the tid-0 registration and the thread closure, and is also returned at `spawn.rs:663`.
  - `mod.rs:2596`
  - amd64 `usermode.rs:4772`: `channel` is moved later at `:4833`.
  - amd64 `usermode.rs:4834`
  - amd64 `usermode.rs:3925`: `exit_fds` must outlive `thread::drain`.
  - `clone_thread`'s `fds`/`signal_actions`/`creds` (`mod.rs:3592-3594`): `CLONE_FILES` sharing *is* the semantics.
  - `inherit_from`'s `stdin`/`stdout`/`terminal_state`/`namespace`/`channel`
  - Taoxichuan `main.rs:3005` and amd64 `fs.rs:563`: two holders of the root fs.
  - glue `container.rs:328`: one per layer.
- **`ProcessMemory::clone` for fork** (`process/types.rs:58`). A forked child needs its parent's free list; only the thread/vfork use in J3 is questionable.
- **Waker clones** (`alarms.rs:137`, glue `poll.rs:559`). A registered waker must be owned.
- **`akuma-net-unix` `lib.rs:848/915/941/954` and glue `unixsock.rs:533/771/787`.** A name stored in two maps, a borrowed input, or a copy out of the table lock. `unixsock.rs:727` is already conditional on the caller asking for an address.
- **glue `fs.rs:3631` (getdents).** The path is copied out of a *borrowed* peek so that `list_dir` runs without the fd lock. This is already the documented fix for the 60 ms `dir_cache` storm.
- **amd64 `fd.rs:1387` (`file_bytes_at`).** It already clones only when `inode == 0`, which is the fixed shape.
- **amd64 `mm.rs:2271` (`snapshot_shared_write`) and glue `mem.rs:335/1532` (msync/munmap snapshots).** The entries stay in the table and the write-back I/O must run with no lock held. With `Arc<str>` paths these become refcount bumps (S1); as `String`s they are required.
- **`akuma-exec` `table.rs:324/345` `buf[..count].to_vec()` and `children.rs:1768` (ps).** The two-phase "collect into a stack buffer with IRQs masked, then allocate with IRQs on" pattern. The allocation is placed outside the masked phase on purpose.
- **`akuma-ext2` `ext2.rs:1757/3475` `to_vec`.** Only reached when the `E2_VERIFY_HITS` oracle is armed (off by default).
- **`akuma-fdtable` `fork_copy`'s `cloexec`/`nonblock` `FlagSet` (`BTreeSet`) clones** (`lib.rs:398-399`). The child gets an independent table.
- **`akuma-syscalls` `slot.rs:410` `world.clone()`.** A model checker's DFS: each branch needs its own world. It is test tooling reachable only from `search()`.
- **`image.rs:141` `fs.path.clone()`.** `ImageInstall::deferred_segments` is a borrowed slice. It becomes movable only if `ImageInstall` becomes owned (F3's fix would make that natural). AArch64-only; amd64 passes `&[]`.
- **API-returns-owned sites.**
  - `akuma-vfs` `memfs.rs:207/227/271`: the `Filesystem` trait returns `Vec`.
  - `mount.rs:363`
  - `akuma-exec-core` `process.rs:167` `clone_data`: `/proc/<pid>/fd/{0,1}` render.
  - `akuma-vfs-glue/src/proc.rs` `to_vec`s: the renders return `Vec`.
  - `akuma-vfs-glue/src/proc.rs:934` `fd_description`: `readlink` needs an owned `String`.

---

## 6. Systemic recommendations for the budget gate

**S1. Make the shared path an `Arc<str>`.**

- Change the four path-carrying types to `Arc<str>`: `KernelFile::path`, `LazySource::File::path`, `SharedWriteBack::path` and glue's `SharedFileMapping::path`.
- This turns F6, F8, F9, J2, J5 and most of §5's snapshot clones into refcount bumps.
- A clone-count gate does not distinguish the two cases. An `Arc` clone and a `String` clone count the same, so the budget measures the wrong thing unless it reports `Arc`-typed sites separately. Clippy's `clone_on_ref_ptr` can do that mechanically: once `Arc::clone(&x)` is enforced, a `.clone()` count becomes a count of non-`Arc` clones.

**S2. Gate on more than `.clone()`.** Use the lexer-aware counter above, with columns for:

- `.clone()`
- `.cloned()`
- `.to_vec()`
- `.to_string()`
- the deep-clone wrappers `get_fd(` / `table_get(`

Otherwise F1 and F2, the two most expensive findings, never move the number. Alternatively, rename `get_fd` to `get_fd_cloned` and give the gate one name to budget.

**S3. Delete the dead or unused sites first, so the starting budget is honest:**

- `is_in_lazy_region` (no callers)
- `FdTable::dup*` (no production callers)
- `msgqueue_push_direct` (`#[allow(dead_code)]`)

**S4. Expected budget deltas if the clear findings land:**

- `amd64`: `.cloned()` −1 (`table_get` retired); `.clone()` −2 (F6) and −1 (F15 `usermode.rs:5739`, `:8863`).
- `akuma-syscalls-glue`: `get_fd` 60 → about 25, plus −5 `.clone()`.
- `akuma-fdtable`: −6 `.clone()`.
- `akuma-isolation`: −2.
- `akuma-mmap`: −2.
- `akuma-exec`: −4 to −8 (F8, F15).
- `taoxichuan`: −2 (F3, F15).
