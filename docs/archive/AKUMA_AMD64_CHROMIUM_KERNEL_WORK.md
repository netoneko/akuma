# Chromium on amd64 Akuma: the kernel work (2026-10-07..08)

`userspace/kami` runs headless Chromium and blits its screencast onto
`/dev/fb0`. It ran first on the trashcan's Ubuntu, in an Alpine container
(Alpine's `chromium` 142, musl). An `strace -f` of one page load there gave the
list of kernel features Chromium leans on (`userspace/kami/README.md` § "What
Chromium asks of the kernel"). This records what that list found missing or
wrong in the amd64 kernel, what was fixed, and what Chromium itself hit when it
first ran on Akuma under Firecracker.

The gate for the fixed half is `userspace/forktest/c_stress/chromeprobe.c`.
Every expectation in it comes from running it on Linux first. On Linux it
passes **16/16**, and on the amd64 kernel under Firecracker **16/16** since
2026-10-08.

## Fix 1: amd64 `sendmsg`/`recvmsg` on a unix fd answered `ENOTSOCK`

`amd64/src/usermode.rs` sent every `sendmsg`/`recvmsg` to `crate::sock`, which
only knows `FileDescriptor::Socket` (AF_INET). A unix fd got `ENOTSOCK`, so
plain unix `sendmsg` had never worked on this target, with or without
ancillary data. Socket, bind, connect and the rest already went "AF_UNIX
first" to glue. These two arms were missed. They now do the same.

## Fix 2: `SCM_RIGHTS` (fd passing) did not exist

Chromium passed 628 descriptors over `SOCK_SEQPACKET`/`SOCK_STREAM` pairs
loading one page. The pure AF_UNIX table had reserved the shape
(`Record::anc_fds`, a channel-teardown hook with a `debug_assert!` that no fds
were in flight), but nothing carried descriptors:

- `sendmsg` ignored `msg_control`.
- `recvmsg` always reported `msg_controllen = 0`.

Now:

- **Wire format.** `akuma_net_unix::scm` holds the LP64 `cmsghdr` parse and
  encode, and Linux's `scm_detach_fds` rule for a short receive buffer. It is
  host-tested.
- **In flight.** Glue's `IN_FLIGHT` token table holds one `clone_fd_refs`
  reference per descriptor in transit. An `InFlight` guard releases them on
  every send path that does not commit.
- **Receive.** `recvmsg` installs the descriptors and honours `MSG_CTRUNC`
  and `MSG_CMSG_CLOEXEC`.
- **Teardown.** Closing a channel that still holds unread descriptors closes
  them, where it used to assert.
- **Stream stop rule.** A `SOCK_STREAM` read stops after the first message
  carrying descriptors (`UnixTable::stream_read_limit`), as Linux's
  `unix_stream_read_generic` does.

Reference: `docs/reference/subsystems/syscalls/net.md` § "SCM_RIGHTS".

## Fix 3: a shared-writable page of an unlinked file was lost when its writer exited

The amd64 write-back (`flush_shared_write`) went through the path recorded at
`mmap`. Once that path stopped naming the inode, it skipped the flush, on the
grounds that "an unlinked file's contents die with its last mapping, as on
Linux". That is not Linux: the contents live as long as *any* fd or mapping
references the inode. The failing sequence:

1. Process A maps an unlinked file and does not touch the page.
2. Process B maps the same file through a passed fd, writes the page, and
   exits.
3. B's exit dropped the shared page (no other present mapper).
4. The flush was skipped, because the file has no name.
5. A's later fault refilled the page from a file that never got B's bytes.

On Linux A sees the write. Chromium unlinks every shared-memory file right
after creating it, so this was on its path.

**Fix:** a by-inode write path (`Filesystem::write_at_by_inode`; ext2
implements it, `SubdirFs` forwards it, `OverlayFs` sends it to the upper layer
only), and the flush uses it whenever the path no longer names the inode. That
also keeps the 2026-10-03 guarantee that an unlink-and-recreate never writes
into the *new* file. Host tests are in `akuma-ext2`; the `shmvar` variants c
and d now match Linux.

## Fix 4: `ftruncate`/`fallocate` on an unlinked fd answered `ENOENT`

Both went by `KernelFile::path`. Chromium creates its shared-memory file,
unlinks it, and only then sizes it. So the file stayed at 0 bytes, and every
later write-back of the mapping fell past EOF and was dropped. They now go by
the fd's inode once the path no longer names it
(`akuma_vfs_glue::{truncate,fallocate}_open_file`, backed by ext2's
`truncate_by_inode`/`fallocate_by_inode`). While the path still names the
inode they keep the path route, so the path-keyed mapping notifications are
unchanged. Gate: `chromeprobe`'s `shm_unlinked_writer_exits`.

## Fix 5: amd64 `setsockopt` on a unix fd answered `ENOTSOCK`

The same routing hole as Fix 1, found by Chromium on Akuma. crashpad's
`setsockopt(SO_PASSCRED)` failed, and the browser `CHECK`-crashed. Glue had
always accepted `SOL_SOCKET` options on unix sockets (`dispatch_setsockopt`),
so AArch64 was unaffected; amd64 now routes there. `SO_PASSCRED` is accepted,
but no `SCM_CREDENTIALS` are generated yet.

## Fix 6: `execve` recorded the literal path as the image name

`/proc/<pid>/exe` reports `image.name`, which `execve` set to the path it was
given. A process started with `execve("/proc/self/exe", …)` (Chromium starts
every child that way) could then read `/proc/self/exe` back as itself, and
Chromium finds its own files next to that path. The name is now resolved
through symlinks before the swap, while `/proc/self` still names the caller.
**Not shown to have changed Chromium's behaviour**: the snapshot and
crashpad errors below persisted after it. *(2026-10-08: they persisted
because the name this fix wrote was overwritten moments later by
`prctl(PR_SET_NAME)`; see Fix 12, which gives `/proc/<pid>/exe` a field of
its own.)*

## Fix 7: `hpbox.deploy()` reported success on a reset that did not land

`deploy()` resets the box to the newest local commit found on *any* remote.
A commit pushed only to the private `litter` remote is chosen even though the
box cannot fetch it. The `git reset` failed inside a pipeline that hid its
status, the box stayed 210 commits back, and the patch was applied onto that
tree. Two Firecracker runs measured a kernel nobody had written: it made
writable `MAP_SHARED` look entirely broken, because the old tree predated the
shared-page table. `deploy()` now refuses when the box did not land on the
intended commit. The runbook (`amd64-bare-metal-loop.md`) shows how to carry
commits over with `git bundle`.

## Fix 8: `execve` copied the whole executable into the kernel heap

The first blocker of the 2026-10-08 handoff. `fs::read_image` read the whole
binary into one heap `Vec` (capped at 256 MB) and handed it to the loader.
Chromium is 250 MB and re-execs itself, and two copies did not fit a 512 MiB
heap (`[ALLOC FAIL] requested=249690856`, then `EIO` after a 20 s wait).

**Fix:** a streaming loader. `akuma_elf::load_elf_eager_from_path` pairs the
crate's existing path source with its eager mapping strategy, and
`map_segment_eager` reads file bytes through a window of 64 KiB (one page on
`extreme`, to keep that profile's 10 KB interpreter-load bound) instead of
one read per page. amd64's `execve`/`spawn` read only the 256-byte head (for
`#!`) through `fs::read_image_head`, then call `loader::load_path`. The
mapping is still eager; lazy file-backed text is a later step.

**Proof:** a 4 GiB guest (512 MiB heap) gets past every Chromium exec. Three
back-to-back execs of the 250 MB binary peaked `Slab:` at 139 MB, of which
128 MB is the block cache (a quarter of the heap), so the exec itself needed
about 11 MB. Host tests in `akuma-elf` (`window_tests`) compare every byte of
a segment spanning several windows, and fail on a mutated refill rule.

## Fix 9: `int3` from ring 3 arrived as `SIGSEGV`

IDT vector 3's gate was DPL 0, so a ring-3 `int3` raised `#GP` (`err=0x1a`)
and became a `SIGSEGV`. Every Chromium `CHECK` failure is an `int3`. The gate
is DPL 3 now (`Entry::set_user`), and `#BP` goes through `ring3_exception` as
`SIGTRAP`/`SI_KERNEL`, as Linux's `do_int3_user` does. Gate: `bpprobe.c`.

## Fix 10: a regular-file `read`/`pread` stopped at 64 KiB

Every file read was clamped to `MAX_IO` (64 KiB), on amd64 and again inside
glue's `File` arms. A short read is legal POSIX, but Linux never gives one on a
regular file before EOF: `pread(fd, buf, 776865, 0)` of the V8 snapshot
returned 65 536 here. Under the amd64 feature `linux-file-io` (default on),
`fd::read_file_full` loops glue's 64 KiB read until the request, EOF or an
error, up to Linux's `MAX_RW_COUNT`. The kernel buffer per call is unchanged.
Not atomic against a second reader of the same description (Linux holds
`f_pos_lock`). Gate: `snapprobe.c`.

## Fix 11: `CLOCK_REALTIME` was frozen at 0 until SNTP set it

`clock_gettime(CLOCK_REALTIME)`, `gettimeofday`, `time` and `adjtimex` all
answered `utc_time_us(..).unwrap_or(0)`. A Firecracker guest with no network
never syncs, so the wall clock read a constant 0, not 0 and counting. musl's
`__randname` (behind `mkdtemp`, `mkstemp`, `tmpnam`) seeds from `tv_sec +
tv_nsec`, so every attempt made the same name: Chromium's
`mkdir("/tmp/.org.chromium.Chromium.scoped_dir.EAAIAA")` failed `EEXIST` 100
times and the ProcessSingleton reported `Failed to create socket directory`.
That was blocker 3. `singletonprobe` passed in isolation because it was the
first process to use the name.

**Fix:** `akuma_primitives::clock::realtime_us`, boot-relative from the epoch
until the clock is set, which is Linux's answer with no RTC. The
userspace-facing readers use it. Kernel checks that must tell "never synced"
from 1970 (certificates, the SNTP retry gate, ext2 timestamps) keep
`utc_time_us`/`is_utc_set`. The futex and `clock_nanosleep` absolute
deadlines already treated an unset clock as uptime, so they now agree with
what `clock_gettime` reports.

## Fix 12: `prctl(PR_SET_NAME)` rewrote `/proc/<pid>/exe`

The cause of blockers 2 and 4 together. `/proc/<pid>/exe` rendered
`ProcessImage::name`, which `PR_SET_NAME` writes. Chromium's browser names
its main thread, so with `strace_nr=89` the same pid read `/proc/self/exe` as
`/usr/lib/chromium/chromium` at startup and as `chromium` a few calls later.
From that it computed its install directory as `""`, and:

- crashpad ran `execve("chrome_crashpad_handler")`, a relative path, which
  failed `ENOENT`. That was blocker 2; `spawnprobe` showed `posix_spawn`
  itself was fine.
- the zygotes ran `execvp("chromium")` through `PATH` (`/usr/local/bin/chromium`,
  `/bin/chromium` → `ENOENT`) and then hit a `CHECK`;
- the V8 snapshot was looked up next to that broken path. That was blocker 4,
  and it was never fd passing.

**Fix:** `ProcessImage::exe`, Linux's `mm->exe_file` beside `name` (`comm`).
It is set at spawn and `execve` on both kernels (the AArch64 `execve` never
refreshed it at all) and inherited on `fork`. Nothing else writes it.
`/proc/<pid>/exe` reads it. Gate: `exeprobe.c` now renames itself and reads
the link again.

## Fix 13: `/proc/<pid>/task` did not exist

After Fix 12 the zygotes reached `sandbox/linux/services/thread_helpers.cc:41
Check failed: . : No such file or directory`. That helper is
`fstatat(proc_fd, "self/task/")` followed by `CHECK_LE(3, st_nlink)`; the
process is single-threaded when `st_nlink == 3`.

**Fix:** procfs serves `<tgid>/task` (the group's live threads) and aliases
`<tgid>/task/<tid>[/rest]` onto `<tid>[/rest]`, because every thread here is a
`Process` with its own per-pid files, so there is no second renderer.
`Metadata` gained `links: Option<u32>` (`None` keeps the old 2/1), and
`stat`/`fstatat`/`statx` report `Metadata::nlink()`, so `task/` says `2 +
threads`. Gate: `taskprobe.c` (nlink 3, then 4 with a second thread, both
tids listed, `task/<tid>/status` reads).

## Fix 14: `brk` fell through the amd64 dispatch

`brk` (x86_64 12) has a row in `akuma-syscalls-abi`, so it decoded, but
`usermode.rs`'s match has no arm for it (the arm was removed when the
allocators moved to `mmap`). It landed in `_ => ENOSYS` with no message:
54 `brk -> -ENOSYS` in one Chromium run. Linux never answers `brk` with
`ENOSYS`; its answer for a break that cannot move is the current break,
unchanged. musl reads that as "no growth" and falls back to `mmap`, which it
already did, now through the documented path. Auditing all 134 rows against
the arms found `Brk` was the only gap. The default arm now prints
`[syscall] x86_64 nr=… decodes to … but has no dispatch arm`, so the next one
cannot be silent.

## Fix 15: `mkdir` ignored its mode

Found with `strace_pid=`: after `mkdir(scoped_dir)` and `stat(scoped_dir)` both
succeeded, the browser went straight into crashpad's dump request. The
`CHECK` between them is `process_singleton_posix.cc`'s: the socket directory's
mode must be exactly 0700, which is `mkdtemp`'s. Glue's `sys_mkdirat` took
`_mode` and never used it, so every directory got the filesystem default,
0755. It now applies `mode & ~umask & 01777`, with the umask `umask(2)` already
reports (`fs::UMASK`, 022; there is no per-process mask yet). That's on both
kernels. Gate: `singletonprobe` now checks the mode; on Akuma it read
`0755 (want 0700) FAIL` before and `0700` after.

`O_CREAT` still applies the caller's mode **without** the umask (open item).

## Fix 16: `SO_PASSCRED` produced no `SCM_CREDENTIALS`

After Fix 15 the browser got far enough to launch child processes through the
zygote, and then `Did not receive ping from zygote child`, six GPU-process
launch failures, and `FATAL: GPU process isn't usable. Goodbye.` The browser
reads a zygote child's ping with `RecvMsgWithPid`, which takes the child's
real pid from `SCM_CREDENTIALS`; `SO_PASSCRED` was accepted and ignored.

**Fix** (`docs/reference/subsystems/syscalls/net.md` § "SO_PASSCRED and
SCM_CREDENTIALS"):

- each unix `Record` carries its sender's credentials, captured at send;
- stream reads stop where the sender changes;
- `recvmsg` writes `SCM_CREDENTIALS` before `SCM_RIGHTS`;
- `getsockopt(SO_PASSCRED)` reports the flag;
- the `pid` in both `SCM_CREDENTIALS` and `SO_PEERCRED` is now the tgid.

Gate: `credprobe.c` (seqpacket, seqpacket with an fd, stream), identical on
Linux and Akuma, plus host tests in `akuma-net-unix`.

Rows added at the same time, all served by glue already: x86_64 128
`rt_sigtimedwait` (crashpad's client waits in it after a dump request), 140
`getpriority`, 141 `setpriority`. The two priority numbers are swapped between
x86_64 and asm-generic.

## The tools that found Fixes 11–16

`docs/runbooks/trace-failing-syscalls-amd64.md`. Three kernel command-line
flags on amd64:

- `strace_err`: one `[sc!]` line per failing syscall, with pid, decoded paths
  and errno. That found Fixes 11 (100 identical `mkdir`s), 12 (the relative
  `execve`) and 14 (`brk`).
- `strace_nr=<n,…>`: also every successful call of those numbers, with
  `readlink`'s target. That found Fix 12's moving `/proc/self/exe`.
- `strace_pid=<n>`: the full trace, restricted to one thread group.

The compile-time `syscall-debug-info` (now forwardable on amd64) added 244
glue lines and no lifecycle lines in the same run, and found nothing the
runtime flags did not. It stays off.

## Still open: what Chromium hits on Akuma now

Runs under Firecracker (`userspace/kami/probe/akuma/`, 4 GiB guest). With
Fixes 8–14 Chromium gets past crashpad, the ProcessSingleton, the V8 snapshot
and the zygote's thread check, and then:

- **A zygote child crashes before it pings**, so the zygote reports `Zygote
  could not fork: … child_pid -1`. crashpad's handler then fails to dump it
  (`ptrace: Function not implemented`, `tgkill: No such process`). The child
  has crashpad's handlers installed, so the fault never printed. Under
  `strace_err` a fault delivered to a handler now prints a `[sig!]` line with
  rip and address, which is the next run.
- **crashpad wants `ptrace`** (`PTRACE_ATTACH` of the crashed process) to
  write a dump. Not needed to render, since a dump is only taken after a
  crash, but every crash is noisier for it.
- **`gettid()` of a main thread is its thread slot, not its pid** (on both
  kernels, by design: `tkill`, futexes and the per-thread arrays index by
  slot). Every Chromium log prefix reads `[<pid>:4:`. On Linux a main thread's
  tid equals its pid, and code that tests `gettid() == getpid()` for "main
  thread" will answer wrong. Not yet shown to be what Chromium trips on.
- **Missing `/proc` and `/sys` files** Chromium reads (each logged as
  `ERROR`, not fatal so far): `/proc/cpuinfo` (`Failed to initialize
  cpuinfo`), `/proc/sys/fs/inotify/max_user_watches`, `/proc/<pid>/oom_score_adj`,
  `/sys/devices/system/cpu/{possible,present,kernel_max}`.
- **Missing x86_64 rows**: 40 `sendfile`, 239 `get_mempolicy`, 297
  `rt_tgsigqueueinfo` (crashpad re-raises a crash signal with it), 444
  `landlock_create_ruleset`; and `inotify_init` (Chromium's file watcher
  logs `Function not implemented`).
- **`O_CREAT` ignores the umask** (a mode of 0666 makes a world-writable
  file); Fix 15 applies it for `mkdir` only.
- **`init=` does not follow symlinks**, and ext2's `read_at` on a symlink
  inode reads the target's bytes as block numbers (`read_sectors: sector
  14819201400`). The rig works around it with `init=/bin/busybox`.
- **The 1324 GiB reservation** costs about 290 ms (Linux: 0.5 ms), nearly all
  of it in `munmap`.

## The whole-file heap (folded in from `proposals/AMD64_FD_WHOLE_FILE_HEAP.md`)

That proposal was the record of an older crash. The second half of it is the
first blocker above, so it lives here now. It was never tracked in git, and
every reference to it points here.

**Found 2026-09-08**, from a photograph of the bare-metal box's framebuffer.
A userspace program that wrote a large file permanently took a core out of the
machine, and on the box that ended the boot:

```
[SSH] Exec: /bin/sh ["-c", "echo AK-OK"]
[OOM] allocation of 268435456 bytes failed
[BKL] stuck: cpu 0 waiting on owner 3
```

Afterwards `ping` answered and port 2222 accepted, but ssh died at
`kex_exchange_identification`. This was the bare-metal "signature B" lockout,
which two hand-offs had dismissed as "not the kernel under test". It recurred
"on binaries that answered fine on the previous boot" because the trigger was
whether that boot wrote a big file. Reproduced on local QEMU by
`i=0; while [ $i -lt 24 ]; do /bin/busybox cat /bin/apk; i=$((i+1)); done > /bigfile`,
which gave one buffer doubling `[HEAP-R] … 67108864->134217728` while the
whole kernel heap tracked that one `Vec`.

**Mechanism.** Three facts composed:

1. `amd64/src/fd.rs`'s `Entry::data` held every open file's **entire
   contents** in the kernel heap until `close`.
2. A write grew it with `Vec::resize`, which **doubles**, so writing N bytes
   briefly needed about 3N of heap.
3. `alloc_error_handler` **halted the core**, behind the single big kernel
   lock.

**Status:**

- **The whole-file cache: fixed** (C2 slice 5, recorded 2026-09-18). `fd.rs`
  caches no contents; reads and writes stream to the VFS in `MAX_IO` (64 KiB)
  chunks.
- **The OOM handler: half fixed.** It releases the BKL unconditionally before
  halting (`amd64/src/main.rs`), so the machine drops to N-1 cores instead of
  stopping. It still halts the core. Killing the faulting process, as AArch64
  does, is still to do.
- **`execve`'s whole-image read: fixed 2026-10-08** (Fix 8). The loader reads
  the file a 64 KiB window at a time, so an exec no longer costs the
  binary's size in heap. `mem.rs`'s heap sizing still describes the old demand
  and can be revisited.

### And a method correction

**`free` cannot see this bug class.** Across the whole 135 MB excursion,
`free` reported the same 1 564 892 KiB before and after. It watches PMM pages,
and this was the kernel heap. The ring-3 leak checks that read `free` around
about 85 process lifetimes are blind to kernel-heap growth. A heap-aware probe
belongs beside them. It is now the `Slab:` line of `/proc/meminfo`
(`akuma_alloc::stats().allocated`, live kernel-heap bytes), which
`scripts/utils/amd64_ring3_check.py` reads before and after its workload.

Background: `userspace/kami/README.md` (the Ubuntu measurements and the
`strace` summary), `docs/reference/subsystems/syscalls/net.md`,
`docs/reference/subsystems/amd64-shared-write-mmap.md`.
