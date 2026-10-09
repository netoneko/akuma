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
2026-10-08. Ten more single-purpose probes beside it (`bpprobe`, `trapprobe`,
`spawnprobe`, `singletonprobe`, `snapprobe`, `taskprobe`, `credprobe`,
`capprobe`, `jitprobe`, `thrprobe`) each pin one fix; `probes.sh` runs them
all. **Outcome, 2026-10-08, Fix 19:** headless Chromium renders a page with
JavaScript on the amd64 kernel under Firecracker.

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

## Fix 17: x86_64 `capget`/`capset` had no row, and every zygote child died on it

The "zygote child crashes before it pings" blocker of the round-2 handoff.
The Linux `strace -f` answers what the child does first: in `--no-sandbox`
mode the zygote forks through
`sandbox::Credentials::ForkAndDropCapabilitiesInChild`, and the child's
sequence is

```
38 set_tid_address(…) = 38
38 rt_sigprocmask(SIG_SETMASK, ~[KILL STOP RTMIN RT_1 RT_2], NULL, 8) = 0
38 rt_sigprocmask(SIG_SETMASK, [CHLD], NULL, 8) = 0
38 capset({version=_LINUX_CAPABILITY_VERSION_3, pid=0}, {effective=0, permitted=0, inheritable=0}) = 0
38 getpid() = 38
38 close(16) = 0
38 sendmsg(11, {… iov_base="CHILD_PING\0", iov_len=11 …}, MSG_NOSIGNAL) = 11
38 read(15, "&\0\0\0", 4) = 4          <- its real pid, from the browser via the zygote
```

(`/root/cdp-probe/linux/smoke.strace` on the trashcan; twelve children,
every one the same). `capset` is wrapped in a `CHECK`
(`Credentials::DropAllCapabilities`), and an official build's `CHECK` prints
nothing. The amd64 table had no row for x86_64 125/126, so the call was
`ENOSYS`. The saved `dmesg` of the previous run already said so, unnamed:

```
[T12.91] [sc!] pid=61 task=42 nr=126 a1=0x7ffffffdd5b8 a2=0x7ffffffdd6f0 a3=0x0 -> -38
[signal] pid=61 killed by signal 6 (default action)
```

one pair per child (pids 61–65, 71, 74–79), each followed by the browser's
`Did not receive ping from zygote child`. Two things hid it:

- the `no row for x86_64 nr=…` print keeps a 32-entry table of numbers it
  has named, and the table was full before the first `capset`, so 126 was
  never printed — only 297 (`rt_tgsigqueueinfo`), which got in earlier;
- the handoff's `[sig!]` logging covers a fault **delivered to a handler**.
  The child died of `SIGABRT` under the default action — not a fault, and
  not handled — which prints the `[signal] … killed by signal 6` line
  instead. It was in the log; nobody was grepping for it. Why the `CHECK`
  ended in `SIGABRT` rather than an `int3` was not traced, because the
  `[sc!]` line two lines above it names the cause.

**Fix:** rows `Capget`/`Capset` (125/126 → asm-generic 90/91) in
`akuma-syscalls-abi`, dispatched to glue's existing arms with the
`setuid`/`setgid` group in `usermode.rs`: `capset` is the accepting no-op
documented in `docs/reference/subsystems/syscalls/proc.md`, `capget`
negotiates the version and reports root's full set. The amd64 self-test
pins both hops and both answers (`capset` → 0, `capget(NULL)` → `EFAULT`).

**Gate: `capprobe.c`**, the child's sequence in miniature — open `/proc`,
`fstatat("self/task/")` nlink 3, `capset(V3, none)`, both `capget(version 0)`
forms, `capget(V3)`, the ping over a `SO_PASSCRED` seqpacket pair — run on
Linux first. That run corrected the probe's own expectation **and glue**:
Linux's `sys_capget` answers an unknown version with a NULL `data` as 0
(version written back, the pure "which version?" probe) and `EINVAL` only
when `data` is given; glue said `EINVAL` for both. It now matches, on both
kernels. The one divergence is printed, not scored: after dropping every
capability, Linux reads back `effective=0` and Akuma reads back root's full
set, because the kernel has no capability model. Chromium never calls
`capget` in the whole Linux run, so nothing of its sees the difference.

## Fix 18: amd64 refused `PROT_WRITE | PROT_EXEC`, which is V8's code range

What the first run after Fix 17 hit. The browser now ran for the whole 400 s
window with its GPU, utility and renderer processes alive, and three
processes (the same renderer, launched three times) died the same way:

```
[T18.50] [sc!] pid=72 task=51 nr=10 a1=0x2a122880000 a2=0x1ffc0000 a3=0x7 -> -22
[Fault] #BP breakpoint in ring 3 on cpu 1 rip=0x00000000177d7f5e … pid=72
```

`mprotect` of a 512 MB range to `PROT_READ|PROT_WRITE|PROT_EXEC`, `EINVAL`,
then a `CHECK`. The Linux trace has the same sequence, succeeding, in every
renderer:

```
66 mmap(0x5943e0000000, 536870912, PROT_NONE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0)
66 prctl(PR_SET_VMA, PR_SET_VMA_ANON_NAME, 0x5943e0000000, 536870912, "v8")
66 mprotect(0x5943e0000000, 536870912, PROT_READ|PROT_WRITE|PROT_EXEC)
66 madvise(0x5943e0000000, 536870912, MADV_DONTNEED)
```

That is V8's code range: reserved `PROT_NONE`, then made RWX as a whole,
with pages flipped between RW and RX from there. (On a host with memory
protection keys V8 does the same through `pkey_mprotect`; this trace has
none, 0 `pkey_*` calls.)

The amd64 kernel refused it twice over. `sys_mmap` and `sys_mprotect` both
answered `PROT_WRITE | PROT_EXEC` with `EINVAL` under a W^X rule ("a JIT is
not something this target supports"), and behind that
`akuma_mmap::Prot::from_prot` mapped any `PROT_WRITE` to `RW_NO_EXEC` — on
AArch64 too, where a RWX request was *granted* and silently lost its execute
bit — and the x86 `PteProt::from_region` turned even the RWX token (`Prot::RW`)
into a non-executable page. Linux grants all of it, and so does every JIT's
expectation.

**Fix:** the two refusals are gone; `Prot::from_prot` keeps `PROT_EXEC`
beside `PROT_WRITE` (the `RW` token, which was already "read, write and
execute" on AArch64); the x86 encoder gained `PteProt::USER_RWX` and maps
`RW` to it, so the only x86 divergence left is the `RO`/`RX` collapse. The
ELF loader's refusal of a writable+executable *segment* is untouched. Every
pin moved with it: the amd64 boot suite (`prot: region RW is executable`,
and the two `mmap`/`mprotect` RWX checks now expect `ESRCH` — accepted, no
process — rather than `EINVAL`), `akuma-mmap`'s `from_prot` table test plus
a new `from_prot_keeps_exec_beside_write`, and `akuma-mmu`'s three x86
encoding pins (`RW` is `0x7`, five distinct encodings, not four).

**Gate: `jitprobe.c`** — V8's four calls on a 512 MB range, code written
into the middle of it and called (42), the page flipped to RX and called
again, and a direct `mmap(PROT_RWX)` page. Linux: 9/9.

## Fix 19: the amd64 thread table held 64 threads, system-wide

After Fix 18 the browser ran 23 s and exited 191: the GPU process died five
times (`GPU process exited unexpectedly: exit_code=5`, `[Fault] #BP` at one
rip), the renderer twice, then `GPU process isn't usable. Goodbye.` Neither
had a failing syscall of its own in the `[sc!]` trace — every `-2` before the
fault (Vulkan ICD and layer directories, `prlimit64(RLIMIT_NPROC)` →
`EFAULT`) fails identically on Linux. The full trace of the GPU process
(`strace_pid=73`) showed the last call before its `CHECK`:

```
[sc>] cpu=1 task=52 nr=56 … flags=VM|FS|FILES|SIGHAND|THREAD|SYSVSEM|SETTLS|PARENT_SETTID|CHILD_CLEARTID|…
[sc] cpu=1 task=52 nr=56 -> 0xfffffffffffffff5          <- clone: EAGAIN
  [clone] thread table full
```

`amd64/src/thread.rs` kept every non-main thread in a fixed array of **64**,
across all processes, with a comment calling that "well past what the
scheduler's `MAX_TASKS` budget makes useful" — the budget is 512. A headless
Chromium is a browser with ~20 threads and a dozen child processes of 5–7
each (the Linux trace: 20 `clone`s in the browser, 5 in the GPU process, 7
in each utility process); the kernel's own high-water line read `80 live
user threads … ceiling=512` at the moment the 65th thread was refused. The
`[sc!]` trace could not show it because it leaves `EAGAIN` out by design;
the `[clone] thread table full` serial line (7 of them in that run) is the
tell, and the runbook now says so.

**Fix:** the table is 448 entries — the task budget less headroom for main
threads — at about 14 KiB of `.bss`. The ceilings that remain are the 512
task slots (each thread also costs one, plus two 32 KiB kernel stacks) and
`akuma-exec`'s 256-row process table, which every `pthread_create` on this
target also takes a row in (collected on exit) and which **panics** rather
than refuses when full. Gate: `thrprobe.c`, 8 processes × 12 threads alive
at once, run on Linux first (96 created, 0 failed); Akuma: the same.

**With Fixes 17–19 Chromium renders.** `chrome-once.sh` exits 0 in about
28 s of guest time, and `/tmp/shot.png` (800×600) reads "kami on Akuma /
JavaScript ran: 6 x 7 = 42" — the goal the round-2 handoff set. The high-water
line of that run is `103 live user threads … ceiling=512`. Standard-image
boot on the same kernel: `731 passed, 0 failed` (727 plus the four new
dispatch checks).

## Fix 20: x86_64 `fallocate` had no row

Chromium sizes its shared-memory files with `fallocate(fd, 0, 0, size)` on a
file it has already unlinked (171 calls in the Linux trace, all `0`). amd64
had no row for x86_64 285, so every call was `ENOSYS` (82 per headless run in
`strace_err`) and Chromium fell back to `ftruncate`: noise, not a failure.
Glue's `sys_fallocate` (by inode, since Fix 4) and ext2's mode-0
preallocation were already there.

**Fix:** row `Fallocate` (285 -> asm-generic 47) in `akuma-syscalls-abi`, an
arm in `amd64/src/usermode.rs` that passes `a1..a4` to glue, and two pins in
the dispatch self-test (the hop, and `fallocate(0,0,0,0)` reaching glue's
`EINVAL` rather than `ENOSYS`). Standard-image boot: `733 passed, 0 failed`
(731 plus those two).

**Gate: `fallocprobe.c`**, run on Linux first. Scored: mode 0 grows an
unlinked file to `offset+len` and the range reads zero, a smaller call never
shrinks it, `len 0` is `EINVAL`, a `MAP_SHARED` write at the far end reads back
through `pread` after `munmap`. Linux: PASS. Akuma under Firecracker: PASS,
and `chrome-once.sh` then logged 0 `nr=285` failures (was 82), exit 0, and
`shot.png` still reads "JavaScript ran: 6 x 7 = 42".

**Two divergences, printed not scored.**

- A `MAP_SHARED` write is not visible to `pread` **until `munmap`** on Akuma
  (Linux: immediately). An `ftruncate`-sized control file behaves the same, so
  this predates `fallocate` and is the mapping layer's, not the row's. Open.
- `FALLOC_FL_KEEP_SIZE` (any non-zero mode) is `EOPNOTSUPP` from ext2 here;
  Linux grants it. Chromium only uses mode 0.

## Open: the CDP screencast delivers an empty frame (found 2026-10-08, on the metal)

`kami` on the trashcan's metal blitted a first frame and the screen went
black. The frame (kept with `KAMI_DUMP`) is a valid 1920x1080 RGBA PNG whose
every pixel is `(0,0,0,0)`. Reproduced under Firecracker with
`probe/akuma/castprobe.py` (kami's own CDP sequence, no framebuffer needed):

| | Linux (Alpine container) | Akuma amd64 |
|---|---|---|
| `Page.captureScreenshot` | 1 shot, opaque, mean RGB (18,35,52) | **identical**, 22161 bytes, opaque |
| first `Page.screencastFrame` | 50267 bytes, alpha 255..255, mean (18,35,52) | **44075 bytes, alpha 0..0, all zero** |
| frames for a static page | 1 | 1 |

(One frame is normal for a static page: the screencast sends on change.)

- Not `kami`: the frame is empty before it is decoded, and `kami`'s blit is the
  row copy `akuma-cli-wgpu` uses; `fbpattern.c` shows a known pattern correctly
  on the panel.
- Not Fix 20: the same probe on a kernel built at `f00fca8f` without the
  `fallocate` row gives the same empty frame.
- Not the background: `Emulation.setDefaultBackgroundColorOverride` (opaque
  white) changes nothing; the frame is not painted, not transparent-over-page.
- Not these flags: `--disable-gpu-compositing`,
  `--use-angle=swiftshader --enable-unsafe-swiftshader`,
  `--disable-features=VizDisplayCompositor` all give the same frame.
  `--in-process-gpu` printed nothing (the probe produced no output; not
  investigated).
- Suspect: the screencast's frames cross from the viz/GPU process to the
  browser through a shared-memory buffer, and the browser reads zeros. The same
  run logs `Corruption detected in shared-memory segment`
  (`persistent_memory_allocator.cc:886`, 71 times), also a cross-process
  shared-memory symptom, and `fallocprobe` found that a `MAP_SHARED` write is
  not visible to `pread` until `munmap`. **Not shown to be the cause.**
- Workaround, in `kami`: it detects an empty first screencast frame and polls
  `Page.captureScreenshot` every 500 ms (`--poll MS` forces it), blitting only
  when the PNG changed. Measured on the metal: not yet.

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

## Fixes 21-24 (2026-10-08, evening): system-font text on Chromium 152

On ryzen's Akuma, `kami https://www.tumblr.com/` painted layout and images but
**no system-font text**. The metal's Chromium was **152.0.7977.82** (Alpine
`latest-stable` via `apk --root`), the Firecracker image's was 142 (Alpine
3.22), which is why nothing on the trashcan had ever shown it. Chromium 152
loads system fonts through a browser-side **FontDataService**: the browser opens
the font, copies it into a shared-memory region (an unlinked temp file, sized
with `fallocate`, mapped `MAP_SHARED` read-write), and the renderer maps a
read-only handle of it. Every step of that was broken for an **unlinked** file.

How it was found (all on ryzen's Firecracker with a Chromium-152 image, Linux
in Docker as the control, each probe run on Linux first):

* `fontcdp.py` (CDP): on Akuma `CSS.getPlatformFontsForNode` was empty and
  `canvas.measureText` width 0 for *every* family, even the explicit "Noto Sans";
  on Linux, Liberation/Noto with real glyph counts. So lookup, not painting.
* `fonttrace.py` (Chromium trace): the renderer got as far as
  `FontDataServiceImpl - sharing memory region`, then `LegacyMakeTypeface` with a
  null family. `Chrome.FontDataService.EmptyPathOnGetFileHandle` is **benign**:
  it fires on Linux too.
* `fontmapprobe.c`: ruled out the bytes (read/mmap/fork of the real font files
  all match on Akuma).
* `shmregionprobe.c`: reproduced the region in miniature.

| Fix | Symptom | Cause | Gate |
|---|---|---|---|
| 21 | 990 `pwritev2` per run -> `EOPNOTSUPP` | Newer musl does `pwrite(2)` as `pwritev2(..., RWF_NOAPPEND)`; every nonzero `RWF_*` was refused. `RWF_NOAPPEND` (0x20) is now accepted as a no-op (positional writes here never append) | `pwv2probe.c` |
| 22 | `pwrite`/`write` to an unlinked file read back as nothing | `sys_pwrite64`/`sys_write` wrote **by path**, `pread` read **by inode**. Both now write by inode (`write_at_open_file_or_path`, `append_open_file`), still telling the path-keyed caches | `pwv2probe.c` |
| 23 | `open("/proc/self/fd/N", O_RDONLY)` of an unlinked file -> `ENOENT` | `/proc/<pid>/fd/N` was an ordinary symlink to the fd's recorded path. When the fd has an inode and its path is gone, `openat` now binds the new fd to that inode (Linux's magic-link behaviour) | `shmregionprobe.c` |
| 24 | A read-only mapping of a file another process maps shared-writable saw the file's old bytes | `fill_file_pages` looked only in the file-page cache and on disk; the writer's unflushed page lives in the shared-writable table (`shmpages`). Read-only fills now take that frame first | `shmregionprobe.c` |

Result: the font test page renders serif, sans-serif, monospace, Helvetica and
emoji under Firecracker with Chromium 152 (screenshot 26 813 B, was 1 489 B).

Still open, found by the probes and not fixed: `read(2)`/`pread` do not see a
writer's unflushed `MAP_SHARED` pages (known); a peer that maps a region
*before* the writer touches it keeps a stale frame (Chromium never does this);
`preadv2` (x86_64 327), `sendfile` (40), `link` (86), `get_mempolicy` (239) have
no rows (`link` is why fontconfig cannot take its cache lock). On real Linux
`RWF_NOAPPEND` on an `O_APPEND` fd still appended (6.17 on overlayfs), which is
why `pwv2probe` prints that case instead of scoring it. One more trap: the
trashcan-era image pins Alpine 3.22; build a `latest` image
(`/root/cdp-probe/new` on ryzen) to see what the metal runs.

## Still open: what Chromium hits on Akuma now

Runs under Firecracker (`userspace/kami/probe/akuma/`, 4 GiB guest). With
Fixes 8–19 Chromium **renders the page** (above). What a successful run
still logs, none of it fatal:

- ~~`nr=285` (`fallocate`) has no x86_64 row~~ — **fixed 2026-10-08, Fix 20.**
  Still open beside it: a `MAP_SHARED` write is invisible to `pread` until
  `munmap`, and `FALLOC_FL_KEEP_SIZE` is `EOPNOTSUPP`.
- **`prlimit64(RLIMIT_NPROC)` → `EFAULT`, 60 per run** — identical on Linux
  (Chromium passes an unmapped `old` on purpose); not a bug.
- **`mremap(p, 4096, 8192, 0)` → `ENOMEM`, ~2900 per run** walking down the
  stack a page at a time — a stack-size probe, identical on Linux (937 lines
  of the same in its trace); not a bug, but it is most of the `[sc!]` output.
- **the 256-row process table** (`akuma-exec`), which every `pthread_create`
  on this target takes a row in and which *panics* when full. A session
  peaked at 103 live user threads; a heavier page may get closer.
- **crashpad wants `ptrace`** (`PTRACE_ATTACH` of the crashed process) to
  write a dump. Not needed to render, since a dump is only taken after a
  crash, but every crash is noisier for it.
- **`gettid()` of a main thread is its thread slot, not its pid** (on both
  kernels, by design: `tkill`, futexes and the per-thread arrays index by
  slot). Every Chromium log prefix reads `[<pid>:4:`. On Linux a main thread's
  tid equals its pid, and code that tests `gettid() == getpid()` for "main
  thread" will answer wrong. Not yet shown to be what Chromium trips on.
- **Missing `/proc` and `/sys` files** Chromium reads (each logged as
  `ERROR`, not fatal so far): ~~`/proc/cpuinfo` (`Failed to initialize
  cpuinfo`)~~ — **written 2026-10-09** (neither kernel had it; shared glue in
  `akuma-vfs-glue::proc` plus the amd64 `cpuid` block renderer
  `amd64/src/cpuinfo.rs`; aarch64 gets bare `processor : N` blocks until it
  registers a renderer; not yet booted), `/proc/sys/fs/inotify/max_user_watches`, ~~`/proc/<pid>/oom_score_adj`~~ (**written 2026-10-09**, Fix 27),
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

## kami on ryzen, 2026-10-09: the GPU process, and why input "did nothing"

Not a kernel fix; what the session found, with the evidence.

- **The GPU process.** `--disable-gpu` still starts a GPU process for
  SwiftShader/ANGLE; on Akuma it fails on every navigation
  (`eglInitialize SwANGLE failed with error EGL_NOT_INITIALIZED`,
  `VK_KHR_surface` not supported, `Exiting GPU process due to errors during
  initialization`) and is respawned. Several times in a row, then the browser
  process died with SIGTRAP ~0.1 s after a navigation committed. Adding
  `--disable-gpu-compositing --disable-software-rasterizer` removed every GPU
  error, and the CDP screencast stopped delivering transparent frames. That
  suggests the GPU process was behind the "Open: the CDP screencast delivers
  an empty frame" entry above too, but that earlier run was not repeated with
  these flags, so it is not established.
  The browser still dies on some cold starts (SIGSEGV, or a SIGTRAP with no
  GPU errors): about one in four, cause not found. `userspace/kami/README.md`
  has the measured flag A/B.
- **Why kami "hung".** It blocked inside each CDP call and, blocked, read no
  keys and drew nothing, so a Chromium that did not answer (a `Page.navigate`
  that never got a reply; a `Page.startScreencast` after a client was
  `kill -9`ed and left its screencast attached) looked like a dead keyboard.
  Fixed in kami (state machine, daemon detaches dead clients' sessions) rather
  than in the kernel. Terminal facts measured on the ssh pty: `tcsetattr` for
  raw mode takes effect (ICANON/ISIG/ECHO/IXON read back cleared), and Enter
  and Ctrl-Q reach the process as `0d` and `11`. The console keyboard path was
  not exercised.
- **Kernel hangs after killing Chromium trees.** Twice on 2026-10-09 the ryzen
  box hung hard: once in a loop of Chromium cold starts with `killall chromium`
  between runs, once right after a single `kill -9` of the browser process of
  a running kami session. The last `klog` of the first ends in `[BKL] stuck:
  owner=2 waiter=7 tag=501 serving=385141856 ... spins=33554432` (tag 501 =
  IRQ/scheduler hold), beside a `[TRAMP-MISMATCH] tid=76 ... stale tid` and a
  `killed by signal 15`; `klog-72.dmesg` on the partition has it. Not
  investigated; the known BKL/scheduler wedge class applies
  (`AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md`, `BKL_VFS_CARVE_OUT.md`).
  Reproduction shape: a multi-process Chromium (zygote, renderer, network and
  storage utilities) running, then its processes SIGKILLed.
- **Per-core clocks.** `Instant::now()` on different cores differs by up to
  about a second; log lines from different threads came out of order. Not
  investigated in the kernel.
- **resolv.conf** listed the QEMU-only `10.0.2.3` first.
- **No Japanese fonts** on the Akuma partition (see the README).

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

## Fix 25 (2026-10-09): the machine-wide pipe cap killed the zygote on a heavy page

**Symptom.** `kami https://tumblr.com` on ryzen: the page loaded, then stopped
responding. Scroll, arrow, Tab and Enter all reached kami (`tty chunk` ->
`input` lines in `/tmp/kami-input.log`) and produced **no frame**: the last
`presented` line was at 118 s and none followed for the ~600 s the log covered.
`/proc` showed no `CrRendererMain` thread and the browser's CPU time was not
moving, i.e. the tab's renderer was gone.

**Evidence** (`/tmp/kami.log`, Chromium 152, +101 s into the session):

    zygote_host_impl_linux.cc:300  Failed to adjust OOM score of renderer with pid 138 ... pid 400   (~40 renderers)
    FATAL:zygote_linux.cc:426] Check failed: . : Too many open files in system (23)
    NOTREACHED hit. Did not receive ping from zygote child
    Failed to send GetTerminationStatus message to zygote

Site isolation gives every cross-site iframe its own renderer, and tumblr.com
has dozens (ads, trackers, embeds). Each renderer is forked by the zygote with
several IPC channels (`socketpair`, `pipe2`).

**Cause.** `amd64/src/pipe.rs`'s `MAX_PIPES = 256`, a machine-wide policy cap
sampled by `at_capacity()` in front of `pipe2` (and by `alloc()` on the spawn
path). A `socketpair(2)` is two glue pipes and counts against it but is not
gated by it, so Chromium's IPC fills the table and the **next `pipe2` answers
`ENFILE`**, which the zygote `CHECK`s. With no zygote nothing can start a
renderer; the tab stays on a dead page. The 256 was sized from a `cargo build
-j8` (peak 66) and said so in its own comment; nothing had measured a browser.

**Fixes.**

1. Kernel: `MAX_PIPES` 256 -> 2048 (commit `cf945f16`). Worst case 2048 full
   pipes is 128 MiB against a 512 MB heap. **Not yet verified live**: it needs
   a kernel rebuild and a boot of the ryzen box, which had not been done when
   this was written. `[PIPES] live= high= refused= cap=` in the 30 s idle block
   will show the real demand.
2. kami, and the reason the kernel fix is not the whole answer: Chromium now
   starts with `--disable-site-isolation-trials --renderer-process-limit=4` and
   `IsolateOrigins,site-per-process` disabled (commit `fead7508`). Verified on
   the box with the **old** 256-pipe kernel, one run: 4 renderers instead of
   ~40, first pixels 15.6 s instead of 25.5 s, `loadEventFired` 31.8 s instead
   of 72.8 s, no zygote FATAL, frames still arriving at 90 s.

**Not the cause, checked.** The 990 ms `decode`/`blit` values in
`kami-input.log` are the per-core clock skew (below), not stalls. `MAX_FDS`
(256 per process) is not it either: the browser held 123 descriptors.

**Also measured the same day (not kami's fault).** Downloads over ryzen's wifi
ran at 65 KB/s from a Mac on the LAN and 66 KB/s from the internet, with TLS
and DNS setup fast (`tls=0.3 s`). `/dev/wifi0`'s link counters read `tx 3640,
stack dropped 1556` and `retry-limit 54`: 43 % of transmitted packets dropped
in the rtw89 TX path. That is why tumblr's load takes tens of seconds even with
the renderers fixed; it is a driver question, not investigated here.

**Method note.** The telemetry already had the answer and no one had read the
*absence* of `presented` lines after the last one; "scrolling is slow" was
"scrolling produces nothing". The first thing to check on a frozen kami is
`ps`/`/proc` for a renderer thread, then the end of `/tmp/kami.log`.

## Fix 28 (2026-10-09): `Failed to adjust OOM score of renderer`

**What it is.** About 45 lines per Tumblr load in `/tmp/kami.log`:

    zygote_host_impl_linux.cc:300  Failed to adjust OOM score of renderer with pid 1353: No such file or directory (2)

**What Chromium does** (read in 142.0.7444.59's `ZygoteHostImpl::AdjustRendererOOMScore`):
on the browser side, for every renderer the zygote forks, it calls
`base::AdjustOOMScore(pid, score)`, which writes the number into
`/proc/<pid>/oom_score_adj`. With the suid sandbox (not used here) it runs the
sandbox helper with `--adjust-oom-score` instead. Failure is `PLOG(ERROR)` and a
return: **never fatal**, the renderer simply keeps the default score.

**Why the web is no help.** Every forum report of this line ends in `Permission
denied` (a non-dumpable renderer, or a missing/unconfigured suid sandbox) and
the line is usually incidental to another crash. Ours ends in `(2)`, ENOENT.

**Fix, and what it did not fix (corrected 2026-10-09, same day).**
`akuma-vfs-glue/src/proc.rs` now serves `<pid>/oom_score_adj`, `oom_adj` and
`oom_score` as constant `0\n` files (listed, stat-able, readable, 0o644, same
box rule as `cmdline`); writes are accepted and discarded. Booted on the metal:
`cat /proc/1/oom_score_adj` reads `0` and `ls /proc/1` lists all three.
**The log line did not go away** (4 in the next run, 83 cumulative in a
`/tmp/kami.log` that is never truncated). The original diagnosis here, "the
line is exactly the missing file", was wrong or at best half of it: the pids it
names (113, 117, 124, 129, 496, 514, 524) are zygote children that **had
already exited** by the time the browser wrote, and their `/proc/<pid>` is only
the `syscalls` stub a dead process leaves behind. So ENOENT means "the child
is gone", i.e. a churn of short-lived zygote children, not a missing file. The
file is still worth having (a live renderer's write now succeeds), and the line
is still non-fatal; what it points at is children dying right after fork, which
is the thing to chase (see the Tumblr entry in `userspace/kami/README.md`).

## Fixes 29-30 (2026-10-09): `mprotect` stranded every page a `PROT_NONE` had hidden

**Symptom.** `userspace/kami/probe/nav_try.py` (two 40-row local pages that link
to each other at the bottom; scroll, `G`, hint, click, repeat): hops 1 and 2
landed in 2 s, **hop 3 never did**, on every run. kami then sat in hint mode:
`Runtime.evaluate` got no reply and the wheel no frame. The same shape had killed
Tumblr's tab after ~100 s (Fix 25). It looked like a hung renderer; it was a
dead one.

**How it was found** (each step is a thing the kernel could not say before, and
is now logged):

1. `/proc` showed no `CrRendererMain`: the renderer was gone, not hung.
2. The kernel logged nothing about the crash, because Chromium's crashpad
   installs a SIGSEGV handler, so a fault is *delivered* and only the handler's
   later death is a `[Fault]`. `deliver_fault_signal` already had a `[sig!]` line
   but only under `strace_err`; it now logs the first 64 per boot always
   (`FAULT_SIGNAL_LOG_CAP`). Result: four zygote children at the same `rip`,
   `sig=11 code=0x2` (`SEGV_ACCERR`), i.e. a write to a *present* page.
3. The `[Fault]` line's `cr2` was **stale** (the handler runs other faults before
   the process dies; it named a library text page for a crash in an allocator
   pool), so the region and PTE for the *real* address (`[sig!]`'s `addr=`) are
   now logged beside it: `[sig!-at] region ... prot=Prot(3)` and
   `[sig!-at] pte write=false exec=false marked=false refs=0`. The region said
   **RW**; the page table said **inaccessible**. (The region lookup has to use the
   address-space *owner*'s table: a `CLONE_THREAD` thread's own `mmap_regions` is
   empty by construction, which first read as "0 regions" and was my probe's
   mistake, not the kernel's.)
4. `rip` was `mov %rsi,(%rdi)` followed by a compare against a pool base:
   PartitionAlloc writing a freelist pointer into memory it had just recommitted.
   Its decommit is `mprotect(PROT_NONE)` (+ `madvise(DONTNEED)`), its recommit
   `mprotect(PROT_READ|PROT_WRITE)`.

**Cause (Fix 29).** `PteProt::from_region(NONE)` is `KERNEL_RO`, i.e.
`user = false`, so `mprotect(NONE)` rewrites a present page to a PTE ring 3
cannot touch. `sys_mprotect`'s leaf rewrite then began with
`if !leaf.prot.user { return LeafAction::Keep; }` — written for kernel pages
that "should never be in a user range" — which also skipped **every page a NONE
had hidden**. The recommit changed the *region* back to RW and left the *page*
inaccessible for good; the next store was a `#PF` protection fault on memory the
region called writable. Every renderer died at its first decommit/recommit cycle
inside a pool, which in the click-through is the third document load.

**Fix 29.** `amd64/src/mm.rs::sys_mprotect`: an inaccessible leaf is skipped only
when the request is itself `PROT_NONE` (nothing to take away); a grant
re-permissions it.

**Fix 30 (same `user` test, other call).** `dontneed_range` had two early
`Keep`s: `!leaf.prot.user`, and "region is `PROT_NONE`". So `madvise(DONTNEED)`
after `mprotect(NONE)` — one of the two orders allocators use — left the old
bytes behind a recommit, where the allocator expects zeros. Both are gone: a
*present* page in a NONE region is memory that was committed and hidden, so
DONTNEED zeroes it like any other (an absent page is never visited, so guard
pages stay frameless); a genuine kernel page is excluded by the region lookup
that follows (it is in no region).

**Verified (ryzen metal, 2026-10-09).**

| | before | after |
|---|---|---|
| `nav_try.py --hops 8` | hop 3 FAILED, 2 of 8, every run | **8 of 8**; 48 inputs, 70 frames, 0 inputs without a later frame |
| `stable_try.py --runs 8` (cold starts, local page, scroll) | 6 of 8 | **8 of 8**, median key->frame 119-144 ms |
| kernel `[sig!]`/`[Fault]` lines during a run | 4+ per run | 0 |
| `decommitprobe` (`userspace/forktest/c_stress`) | `A first page writable FAIL`, then dies | 36 checks ok, PASS |

Re-verified the same evening by a second session against the same boot:
`nav_try` 8/8 again, `stable_try` 8/8 by line order (the probe's scorer was
timestamp-based and produced one false FAIL from cross-core skew; fixed in
`scroll_try.py`), 0 `[sig!]`/`[Fault]`. Transient `[BKL] stuck` lines (tags
501/502/11/35, all resolved) appeared during cold starts at `smp=8`; see
`userspace/kami/README.md` § "Stable on local files". Not yet run under
Firecracker (both Firecracker hosts were booted into Akuma).

The probe's negative control is the pre-fix kernel binary (hash `b584646b…`),
restored afterwards (`371388ea…`). **No Linux control was run**; see the probe's
header. A heal-on-fault in `cow_write_fault` was tried first and removed: it
never fired because that handler bails out on `!prot.user` before any decision,
which was itself the clue that the PTE was a NONE leaf.

**Not fixed, and now separable from this:** Tumblr still paints one frame and
stops. The renderer is alive; the *network service* crashes ~3-5 s into every
session (`Network service crashed or was terminated, restarting service`, once
per Chromium start, with a `sig=5` / `int3` CHECK from the main binary in the
kernel log, next to `CreatePlatformSocket() failed: Address family not supported
by protocol (97)`), and local files never need it. Chasing that is the next step.
A guess worth testing rather than believing: the earlier minutes-long wedges
after a hung tree was killed (below) may have been crashpad's `tgkill` flood from
dead renderers, which these fixes stop producing.

**Fix 31 (2026-10-09, evening). `amd64/src/fd.rs::MAX_FDS` 256 -> 1024.** Glue's
`prlimit64` has answered `RLIMIT_NOFILE` = 1024 since C1 step 3; the table's
lookup bound stayed at 256. Under Firecracker with the tap NIC
(`userspace/kami/probe/akuma/run-net.sh`, `strace_err`), tumblr.com drove the
network service and four other processes into `socket(2) -> EMFILE` 200+
times in 15 s, every local page far below the cap. The table is a `BTreeMap`,
so 1024 costs only what a process holds. Verified: the same run on the new
kernel logs 0 `EMFILE`; the standard self-test suite still boots. Not verified
to be what kills the metal's browser — it did not die under Firecracker on
either kernel.

## Fixes 32-34 (2026-10-09, night): the network service's CHECK, exec'd altstacks, timed waits

**What kills the network service on the metal (found).** ryzen boot 89 (HEAD
`9e2f4669` built `no-tests`, entry 14, `smp=8`, netwatch off): three cold
`page_try.py https://www.tumblr.com/` runs each "survived" and each saved a
**blank white** final frame; `kami.log` shows `Network service crashed or was
terminated, restarting service` ~4 s into every Chromium start, and the kernel
log (`klog-89.all`) one `[sig!] sig=5 code=0x80 rip=0x19157a3a -> handler`
per run, same `rip` every time. Chromium is loaded at `0x10000000`, so the
trap is the `int3` at file vaddr `0x9157a39`, a CHECK-failure stub reached from
`cmp %r14,%rcx; jg` at `0x915765d`: `CHECK(a <= b)` on two fields that the code
then subtracts with saturation (`TimeTicks - TimeTicks`). The function's
literals are `Net.NetworkTransaction.StreamRequestCompleteTime4`,
`…NegotiatedProtocol3`, `…StreamAddressFamily3`, `…StreamRequestErrorCode4`:
`HttpNetworkTransaction` recording its stream-request timing, and the CHECK is
"the stream request did not end before it started". (Disassembled on the box
with Alpine's `objdump`; the binary is stripped, so the name comes from the
strings, not a symbol.) Adding `/etc/hosts` (missing on p3) changed nothing.

**Fix 32: per-core TSC offset.** `lapic::tsc_uptime_us` (= `CLOCK_MONOTONIC`)
subtracted the BSP's `TSC_START` from whichever core's `rdtsc` the caller ran
on. KVM keeps vCPU TSCs together; the metal's cores did not (`wakelat`: a 1 ms
spin read as -1 011 819 us / +1 019 192 us across cores), so a network-service
thread that migrated between its two `TimeTicks::Now()` saw the end precede
the start. That is why it reproduced on the metal 3/3 and under Firecracker
0/6. Now `smp::start_secondaries` runs a 64-round `rdtsc` ping-pong with each
AP as it comes online (`lapic::tsc_sync_bsp`/`tsc_sync_ap`, shortest round trip
wins, as Linux's `tsc_sync` estimates), stores the AP-minus-BSP offset per
core, prints `smp: cpu N tsc offset X cycles`, and the clock subtracts it.
**Status: verified on the metal, boot 90 (below); QEMU `SMP=4` offsets 0-1
cycles.**

**Fix 33: `execve` left the per-thread altstack in place.** `install_image`
cleared `Process::sigaltstack_*`, but frames are placed from the per-slot
`threading::get_sigaltstack`, which `fork` copies and exec (same slot) kept. A
fork+exec'd Chromium helper therefore inherited an altstack address from the
browser's image; in the new image that VA is whatever got mapped there, which
fits the declined `sig 5` frame of boot 88 (`frame write to 0x100002e38
failed`, a read-only file mapping). Now `install_image` also clears the
calling slot's altstack (both exec paths call it on the exec'ing thread).
Probe: `userspace/forktest/c_stress/altstackexec.c` (fork keeps it, exec
disables it, pthreads after 64 threads of altstack churn start disabled; each
case raises `SIGTRAP` into a `SA_ONSTACK` handler). **Linux: all PASS**
(arm64 kernel 7.0 in Lima `fc`, and Pop on the ryzen). **Akuma: FAIL on the
old kernel, PASS on the fixed one** (boot 90, below).

**Fix 34: timed waits wake at their deadline, not the next tick.** Two
causes of the extra tick `timerlat` measured:
1. `futex::wait` called `allow_tick()` (`sti; hlt; cli`) at the top of
   **every** pass of its loop, *before* testing the deadline, so after the
   wake pass readied the thread at the first tick past its deadline it halted
   to the *next* tick before noticing (16 ms -> tick at 20 -> readied -> halt to
   30; median 26.3 because threads resume on cores with out-of-phase ticks).
   That halt exists for the tick clock, which stops with `IF` clear; with the
   TSC calibrated it now does not run.
2. Even served promptly, a deadline waits for a halted core's next periodic
   tick. `lapic::arm_deadline` (called before the `hlt` in `idle_loop` and
   `allow_tick`) switches the core's LVT timer to one-shot for the earliest
   `WAKE_TIMES` deadline of any `WAITING` thread
   (`akuma_threading::x86_earliest_wake_time`) when that is sooner than the
   current tick; `restore_periodic` puts the periodic tick back after the halt.
   Only when the TSC is calibrated, so the tick count is not the clock.
**Status: verified on the metal, boot 90 (below).**

**The wake IPI** (same night). `ThreadWaker::wake`'s `trigger_sgi` hook was a
no-op on amd64, so a thread readied for a halted core waited for that core's
next tick (`wakelat` `futex+busy` p90 1.9 ms on the metal). Now it is
`smp::kick_halted_core`: claim one halted core (per-core `HALTED` flag, CAS)
and send it vector 34, whose handler only EOIs. The idle loop sets the flag
before a last non-switching look for work (`akuma_threading::x86_runnable_exists`)
and clears it after the `hlt`, so a wake cannot fall between look and halt.
QEMU `SMP=4`: 817 passed, 0 failed; on the metal `futex+busy` p90 14 us (below).

### Verified on the metal (boot 90, 2026-10-09 ~19:53 UTC)

Kernel `6f8934d5`, **built inside Akuma on the ryzen itself** (`kbuild -j 4
--features no-tests` in a worktree, 27 s incremental), installed from Pop,
entry 14 (`smp=8`), netwatch off.

- **The skew, measured:** `smp: cpu N tsc offset 3842016277..3842016486
  cycles` for all seven APs. At 3793 MHz that is **1.013 s**: every AP's TSC
  runs 1.013 s ahead of the BSP's, and the APs agree with each other to ~200
  cycles. That is the ±1.01 s `wakelat` saw last round.
- **Tumblr:** `page_try.py https://www.tumblr.com/ --runs 2`: top frame
  committed, first pixels 9.3 s, **`Page.loadEventFired` 9.7 s**, final frames
  405/409 KB showing the Trending page (posts, images, sidebar, the cookie
  dialog). **No `Network service crashed` line and no Chromium `int3` this
  boot** (boot 89: one per Chromium start, 3/3 blank pages).
- **`altstackexec`:** all PASS (boot 89, same binary: `exec'd image altstack
  FAIL (flags=0 sp=0x5a5a00000000)`, child killed by its own handled SIGTRAP).
  Linux on the same CPU (Pop): all PASS.
- **`timerlat` (N=100), median overshoot, before -> after:** futex-bitset
  1 ms 8999 -> **13 us**, 5 ms 4999 -> **14 us**, 16 ms 10319 -> **12 us**;
  nanosleep 1/5/16 ms 573/1343/361 -> **22/23/23 us**; epoll_pwait
  532/1341/347 -> **26/26/28 us**; ppoll 341 -> **27 us**. Linux on the same
  CPU (Pop, host kernel 6.17): futex-bitset 1/5/16 ms overshoot 570/574/553 us
  (the "+0.12 ms" quoted earlier was a different Linux run).
- **`wakelat`:** `futex+busy` p90 1.9 ms (last round) -> **14 us** (Linux
  5 us); `nanosleep(100 us)` median 1101 -> **105 us**.
- **Frame latency:** `stable_try.py --runs 6` **6/6 PASS, medians 74-78 ms**
  (was 109-144, median 120; Linux 72). `scroll_try.py`: isolated keys median
  74 ms (max 113), bursts 73 ms (max 85), 9 frames/s, no key without a frame.

## Scoreboard: kernel bugs found by running kami / Chromium

Kept here so the count survives; update the row, not the prose, when something
changes. "Outside" means the only record is somewhere other than `docs/archive/`
and should be linked from here (done in the last column).

| # | Bug | Status | Record |
|---|---|---|---|
| 1 | amd64 `sendmsg`/`recvmsg` on a unix fd: `ENOTSOCK` | fixed 10-08 | this doc, Fix 1 |
| 2 | no `SCM_RIGHTS` | fixed 10-08 | Fix 2; also `docs/reference/subsystems/syscalls/net.md` (outside) |
| 3 | shared-writable page of an unlinked file lost at writer exit | fixed 10-08 | Fix 3; also `docs/reference/subsystems/amd64-shared-write-mmap.md` (outside) |
| 4 | `ftruncate`/`fallocate` on an unlinked fd: `ENOENT` | fixed 10-08 | Fix 4 |
| 5 | amd64 `setsockopt` on a unix fd: `ENOTSOCK` | fixed 10-08 | Fix 5 |
| 6 | `execve` recorded the literal path as the image name | fixed 10-08 | Fix 6 |
| 7 | `hpbox.deploy()` reported a reset that never landed (tooling, not kernel) | fixed 10-08 | Fix 7 |
| 8 | `execve` copied the whole binary into the kernel heap | fixed 10-08 | Fix 8 |
| 9 | `int3` from ring 3 arrived as `SIGSEGV` | fixed 10-08 | Fix 9 |
| 10 | regular-file `read`/`pread` stopped at 64 KiB | fixed 10-08 | Fix 10 |
| 11 | `CLOCK_REALTIME` frozen at 0 until SNTP | fixed 10-08 | Fix 11 |
| 12 | `prctl(PR_SET_NAME)` rewrote `/proc/<pid>/exe` | fixed 10-08 | Fix 12 |
| 13 | no `/proc/<pid>/task` | fixed 10-08 | Fix 13 |
| 14 | `brk` fell through the amd64 dispatch | fixed 10-08 | Fix 14 |
| 15 | `mkdir` ignored its mode | fixed 10-08 | Fix 15 |
| 16 | `SO_PASSCRED` gave no `SCM_CREDENTIALS` | fixed 10-08 | Fix 16 |
| 17 | x86_64 `capget`/`capset` had no row | fixed 10-08 | Fix 17 |
| 18 | `PROT_WRITE\|PROT_EXEC` refused (V8 code range) | fixed 10-08 | Fix 18 |
| 19 | thread table held 64 threads system-wide | fixed 10-08 (448) | Fix 19 |
| 20 | x86_64 `fallocate` had no row | fixed 10-08 | Fix 20 |
| 21-24 | system-font text on Chromium 152 (FontDataService, unlinked-file mapping chain) | fixed 10-08 | Fixes 21-24 |
| 25 | **`MAX_PIPES` 256 starved the zygote: `ENFILE`, dead renderer** | **fixed in tree 10-09, not verified live** | Fix 25 |
| 26 | `kill(2)` took the AArch64 hard-kill path: BKL wedge after a Chromium tree `kill -9`, 2 s per threaded kill, leaked thread rows | fixed 10-09 | `AKUMA_AMD64_SIGKILL_NATIVE_PATH.md`; symptom row in `docs/README.md` |
| 27 | `/proc/cpuinfo` did not exist on either kernel (`Failed to initialize cpuinfo`) | **fixed and booted on the metal 10-09** (Ryzen 7 8845HS, family 25 model 117, 8 blocks) | "Still open" list above; glue `proc.rs`, `amd64/src/cpuinfo.rs` |
| 28 | `/proc/<pid>/oom_score_adj` (+ `oom_adj`, `oom_score`) did not exist | **fixed and booted 10-09**, but it was *not* the cause of the `Failed to adjust OOM score` log line (still printed: the named children are already dead) | Fix 28 above |
| 29 | **`mprotect` skipped every page a `PROT_NONE` had made `user = false`, so a recommit never took**: PartitionAlloc's pool pages stayed inaccessible, every renderer died at its first decommit/recommit (the third document load) | **fixed and booted 10-09; `nav_try` 8/8, `stable_try` 8/8, `decommitprobe` PASS (negative control FAIL)** | Fixes 29-30 above |
| 30 | `madvise(DONTNEED)` skipped present pages of a `PROT_NONE` region and `user = false` pages: old bytes behind a recommit | **fixed and booted 10-09** (`decommitprobe`) | Fixes 29-30 above |
| 31 | **`MAX_FDS` 256 while `prlimit64` answers `RLIMIT_NOFILE` 1024**: Chromium's network service took `EMFILE` from `socket(2)` 200+ times in 15 s of tumblr.com under Firecracker; a `socketpair`/`pipe2` refused with `EMFILE` is a CHECK in its IPC layer | **fixed in tree 10-09 (1024), booted under Firecracker, 0 `EMFILE` after; not shown to be the metal's killer** | Fix 31 below |
| 32 | **`CLOCK_MONOTONIC` differs between cores by up to ~1 s on the metal** (no per-core TSC offset): Chromium's network service CHECKs `start <= end` on a stream request and died ~4 s into every Chromium start on the metal, so tumblr painted a blank page | **fixed and verified on the metal 10-09** (APs +1.013 s vs BSP; tumblr loads, 0 network-service crashes) | Fixes 32-34 above |
| 33 | **`execve` kept the per-thread `sigaltstack`** (only the `Process` copy was cleared): an exec'd helper's signal frames aimed at the parent image's altstack VA | **fixed and verified on the metal 10-09** (`altstackexec` FAIL -> PASS; Linux PASS) | Fixes 32-34 above |
| 34 | **timed futex waits overshot by a whole tick** (an `hlt` before the deadline test on every pass, and no one-shot timer) | **fixed and verified on the metal 10-09** (futex 1 ms: 10.0 -> 1.013 ms; key->frame 120 -> 74-78 ms) | Fixes 32-34 above |

Score: 30 fixes (Fixes 1-25, where 21-24 share a row, plus the SIGKILL path,
the two `/proc` files, and the two `mprotect`/`madvise` fixes), of which Fix 7 is
tooling, so **29 kernel bugs**; 28 verified live, **1 (25) in tree and booted but
not shown to be what unblocks Tumblr**; 7 open items below. Plus 31-34
(2026-10-09 evening/night) and the wake IPI, all verified live on the metal
(boot 90) except 31's role there.

**Open, found by the same work, not fixed:**

| Bug | Record |
|---|---|
| ~~per-core clocks differ by up to ~1 s~~ — **fixed 10-09 as Fix 32**, verified on the metal (APs +1.013 s) | Fixes 32-34 above |
| a `MAP_SHARED` write is invisible to `pread` until `munmap`; `FALLOC_FL_KEEP_SIZE` is `EOPNOTSUPP` | "Still open" above |
| 256-row process table panics when full | "Still open" above |
| wifi TX path drops 43 % of packets, ~65 KB/s ceiling | Fix 25 above; `userspace/kami/README.md` § "Tumblr: why scrolling stopped" (outside) |
| ~1 in 4 Chromium cold starts die (SIGSEGV/SIGTRAP), cause unknown | "kami on ryzen, 2026-10-09" above |
| **sshd corrupts a large stdout stream, non-deterministically**: `cat` of the 9.16 MB kernel over ssh gave 3 different SHA-256s in 3 runs (`d555…`, `1228…`, one correct) and once 10 KB more bytes than the file; the same file through local pipes on Akuma (`cat \| sha256sum`, `dd \| sha256sum`) was correct 4 of 4, and a plain HTTP `curl -T` upload of it was byte-identical. Not wire corruption (TCP checksums), so it is in sshd or the kernel's socket/pipe path before the NIC. Use HTTP for binaries until it is found | 2026-10-09; no investigation yet |
| ~~**A renderer stops answering CDP on the third document load**~~ — **fixed 2026-10-09 (Fixes 29-30)**: the renderer was dead, killed by an inaccessible page under an RW region | closed |
| **Killing a hung Chromium tree wedges the box's userspace for minutes**: after that hang, `kami --kill; killall chromium` did not return for 90 s+, `ssh` commands hung for ~4 min (recovered by itself, same boot), and the second time for 15+ min (ping and the ssh port still answered, high latency, no command ran). The known class (`[BKL] stuck ... tag=501` after `kill -9` of a Chromium tree, see "kami on ryzen" above); this kernel carries the 2026-10-09 native SIGKILL path, which did not cure it for a *hung* tree | 2026-10-09; needs a reboot to recover |
| **Tumblr loads nothing on the 2026-10-09 kernel** (**fixed 10-09 night: a `TimeTicks` CHECK in `HttpNetworkTransaction` tripped by cross-core clock skew; Fix 32; tumblr loads on the metal, boot 90**): navigation commits, one first frame, then no frames and no load event; `Network service crashed or was terminated, restarting service` ~3 s after the page starts (crashpad's `ptrace: Function not implemented` + a `tgkill` flood follow), and the restarted service logs `CreatePlatformSocket() failed: Address family not supported by protocol (97)`. A local `file://` page renders and scrolls fine (121 ms key->frame). Not yet separated into: this kernel (it also carries the AF_UNIX-park and kill-path commits of 2026-10-09 that the earlier, working kernel lacked), the new renderer-cap flags, or a flaky network service | `userspace/kami/README.md` § "Tumblr: why scrolling stopped" |
| missing rows/files list (`sendfile`, `inotify_init`, `/proc/cpuinfo`, ...) | "Still open: what Chromium hits" above |
| ~~**Timed futex waits overshoot by a full 10 ms tick**~~ — **fixed 10-09 as Fix 34, verified on the metal: 13 us** — (`FUTEX_WAIT_BITSET` abs 1 ms -> 10.0 ms median, 16 ms -> 26.3 ms; Linux +0.12 ms): every `pthread_cond_timedwait` in Chromium's frame pipeline pays it, which is most of kami's 120-160 ms key->frame against Linux's 72 ms on the same CPU | `userspace/kami/README.md` § "Frame latency: where the 120 ms goes"; probes `userspace/forktest/c_stress/{wakelat,timerlat}.c` |
| **A fault signal's handler frame can be declined** (**fixed 10-09 as Fix 33**, exec kept the per-thread altstack; reproduced and verified fixed on the metal with `altstackexec`): `[signal] sig 5 declined: frame write to 0x100002e38 failed` on the metal — the alternate stack the kernel resolved for the thread lay in a read-only file mapping, so crashpad never ran and the process died on the trap. If that can happen to a `SIGSEGV` V8 or WebAssembly handles on purpose, it is a browser death | metal klog boot 88, 2026-10-09; `amd64/src/signal.rs` frame placement, `akuma_threading::get_sigaltstack` |
| `[unregister] pid=N stale tid=T now owned by pid=N+1` / `[TRAMP-MISMATCH]` for every short-lived process in a fork+exec loop (25 lines for 24 `tr`/`cut` runs) — thread-slot ownership lags process exit | Firecracker runs 2026-10-09 (`run-net.sh`) |
| "exit status 191" from a browser process 4 s after tumblr committed on the metal; meaning not established (neither a Chromium result code nor a kernel status encoding anyone found) | metal `/tmp/kami.log` 2026-10-09 21:03 |
| **`akuma-ext2` host test `rewrite_truncate_rename_and_concurrent_append_keep_files_apart` fails intermittently** (`e2fsck -fn found problems`, `tests.rs:2627`): 1 in 6 full-suite runs once, then 0 in 15; it failed the pre-commit hook on a docs-only commit. Predates this work (test last changed in `ef2bc901`). An `e2fsck` complaint under concurrent append may be a real ext2 race, not test noise | 2026-10-09 night; not investigated |
| **The metal's tumblr deaths did not reproduce under Firecracker** in six runs (4/8 vCPUs, kami itself, a netem-throttled lossy tap), on both the 256- and 1024-descriptor kernels | `userspace/kami/README.md` § "Tumblr under Firecracker" |

**Written outside this directory** (so the score is not lost): the tumblr
measurements and the renderer-cap flags are in `userspace/kami/README.md`
§ "Tumblr: why scrolling stopped"; the remaining kernel work is laid out in
`docs/handoff-kernel-chromium-support.md`; the symptom rows are in
`docs/README.md`.
